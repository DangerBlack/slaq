//! Terminal front end (ratatui).
//!
//! Rendering and input only: state and actions live in [`crate::app`], shared
//! with the graphical front end.

use crate::app::{
    filtered_channels, format_ts, App, AppState, ChannelEntry, MessageEntry,
    ViewMode,
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io;

/// Starts the TUI and returns when the user quits.
pub async fn run(app: &mut App) -> Result<()> {
        let mut stdout = io::stdout();
        crossterm::terminal::enable_raw_mode()?;
        let backend = CrosstermBackend::new(&mut stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;


        app.spawn_bootstrap();

        let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                match event::read() {
                    Ok(Event::Key(key)) => {
                        if key_tx.send(key).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        // Real-time listening. Messages land in the cache as they arrive, so a
        // restart does not have to download them again.
        let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel(256);
        let ws_creds = app.creds().clone();
        tokio::spawn(async move {
            crate::ws::listen_forever(ws_creds, None, ws_tx, None).await;
        });

        let mut ticker = tokio::time::interval(tokio::time::Duration::from_millis(16));

        loop {
            tokio::select! {
                Some(key) = key_rx.recv() => {
                    let mut state = app.state().write().await;

                    // A mod dialog, when up, owns the keyboard: ↑/↓ pick an action,
                    // Enter runs it (or dismisses on the trailing "Dismiss" row),
                    // Esc dismisses. A mod cannot trap the user.
                    #[cfg(feature = "mods")]
                    if let Some(dialog) = state.mod_dialog.clone() {
                        // One extra row is the always-available Dismiss.
                        let n = dialog.actions.len() + 1;
                        match key.code {
                            KeyCode::Esc => {
                                state.mod_dialog = None;
                                state.mod_dialog_cursor = 0;
                            }
                            KeyCode::Up => {
                                state.mod_dialog_cursor = state.mod_dialog_cursor.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                if state.mod_dialog_cursor + 1 < n {
                                    state.mod_dialog_cursor += 1;
                                }
                            }
                            KeyCode::Enter => {
                                let selected = dialog
                                    .actions
                                    .get(state.mod_dialog_cursor)
                                    .map(|(id, _)| (dialog.mod_id.clone(), id.clone()));
                                state.mod_dialog = None;
                                state.mod_dialog_cursor = 0;
                                drop(state);
                                match selected {
                                    Some((mid, aid)) => App::invoke_mod_dialog_action(
                                        app.state().clone(),
                                        mid,
                                        aid,
                                    ),
                                    None => App::close_mod_dialog(app.state().clone()),
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // The Mods consent dialog, when up, owns the keyboard.
                    #[cfg(feature = "mods")]
                    if state.mods_consent.is_some() {
                        match key.code {
                            KeyCode::Esc => {
                                drop(state);
                                App::dismiss_mods_consent(app.state().clone());
                            }
                            KeyCode::Char('a') => {
                                drop(state);
                                App::apply_mods_consent(app.state().clone());
                            }
                            KeyCode::Up => {
                                if let Some(d) = state.mods_consent.as_mut() {
                                    d.cursor = d.cursor.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(d) = state.mods_consent.as_mut() {
                                    let n = consent_cap_count(d);
                                    if d.cursor + 1 < n {
                                        d.cursor += 1;
                                    }
                                }
                            }
                            KeyCode::Char(' ') | KeyCode::Enter => {
                                if let Some(d) = state.mods_consent.as_mut() {
                                    toggle_consent_cap(d);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // The Mod settings form, when up, owns the keyboard.
                    #[cfg(feature = "mods")]
                    if state.mods_settings.is_some() {
                        match mod_settings_keys(&mut state, key) {
                            SettingsAction::None => {}
                            SettingsAction::Apply => {
                                drop(state);
                                App::apply_mods_settings(app.state().clone());
                            }
                            SettingsAction::Dismiss => {
                                drop(state);
                                App::dismiss_mods_settings(app.state().clone());
                            }
                        }
                        continue;
                    }

                    // F3 opens the Mods dialog (review/revoke); F4 the settings form.
                    #[cfg(feature = "mods")]
                    if key.code == KeyCode::F(3) {
                        drop(state);
                        App::open_mods_consent(app.state().clone());
                        continue;
                    }

                    #[cfg(feature = "mods")]
                    if key.code == KeyCode::F(4) {
                        drop(state);
                        App::open_mods_settings(app.state().clone());
                        continue;
                    }

                    // F5 runs the first declared composer action on the current
                    // draft (a picker is a follow-up).
                    #[cfg(feature = "mods")]
                    if key.code == KeyCode::F(5) && !state.mod_composer_actions.is_empty() {
                        let target = state
                            .channels
                            .get(state.selected_channel)
                            .map(|c| c.id.clone());
                        let first = state
                            .mod_composer_actions
                            .first()
                            .map(|a| (a.mod_id.clone(), a.action_id.clone()));
                        if let (Some((mid, aid)), Some(ch)) = (first, target) {
                            let draft = state.input.clone();
                            drop(state);
                            App::invoke_mod_composer_action(app.state().clone(), mid, aid, ch, draft);
                            continue;
                        }
                    }

                    // The mod-action menu, when up, owns the keyboard.
                    #[cfg(feature = "mods")]
                    if state.mod_action_menu.is_some() {
                        match key.code {
                            KeyCode::Esc => state.mod_action_menu = None,
                            KeyCode::Up => {
                                if let Some(m) = state.mod_action_menu.as_mut() {
                                    m.cursor = m.cursor.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                let n = state.mod_actions.len() + state.mod_context_actions.len();
                                if let Some(m) = state.mod_action_menu.as_mut() {
                                    if m.cursor + 1 < n {
                                        m.cursor += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let sel = state.mod_action_menu.as_ref().and_then(|m| {
                                    state
                                        .mod_actions
                                        .iter()
                                        .chain(state.mod_context_actions.iter())
                                        .nth(m.cursor)
                                        .map(|a| {
                                            (
                                                a.mod_id.clone(),
                                                a.action_id.clone(),
                                                m.channel.clone(),
                                                m.ts.clone(),
                                            )
                                        })
                                });
                                state.mod_action_menu = None;
                                if let Some((mid, aid, ch, ts)) = sel {
                                    drop(state);
                                    App::invoke_mod_action(app.state().clone(), mid, aid, ch, ts);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // An open mod-view panel: Esc closes it.
                    #[cfg(feature = "mods")]
                    if state.mod_view_panel.is_some() {
                        if key.code == KeyCode::Esc {
                            state.mod_view_panel = None;
                        }
                        continue;
                    }

                    // The mod-view picker, when up, owns the keyboard.
                    #[cfg(feature = "mods")]
                    if state.mod_view_menu.is_some() {
                        match key.code {
                            KeyCode::Esc => state.mod_view_menu = None,
                            KeyCode::Up => {
                                if let Some(m) = state.mod_view_menu.as_mut() {
                                    m.cursor = m.cursor.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                let n = state.mod_views.len();
                                if let Some(m) = state.mod_view_menu.as_mut() {
                                    if m.cursor + 1 < n {
                                        m.cursor += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let sel = state.mod_view_menu.as_ref().and_then(|m| {
                                    state.mod_views.get(m.cursor).map(|v| {
                                        (v.mod_id.clone(), v.view_id.clone(), v.label.clone())
                                    })
                                });
                                state.mod_view_menu = None;
                                if let Some((mid, vid, label)) = sel {
                                    drop(state);
                                    App::open_mod_view(app.state().clone(), mid, vid, label);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if state.view == ViewMode::Chat {
                        match key.code {
                            KeyCode::Esc => {
                                state.view = ViewMode::ChannelList;
                                state.status = t!("tui.channels", n = state.channels.len()).to_string();
                                state.input.clear();
                            }
                            KeyCode::F(2) => app.open_huddle(&mut state),
                            #[cfg(feature = "mods")]
                            KeyCode::F(6) => {
                                // Open the mod-action menu for the message at the
                                // cursor (the same one the thread key targets).
                                if !state.mod_actions.is_empty()
                                    || !state.mod_context_actions.is_empty()
                                {
                                    let idx = state
                                        .messages
                                        .len()
                                        .checked_sub(1 + state.message_scroll);
                                    let ch = state
                                        .channels
                                        .get(state.selected_channel)
                                        .map(|c| c.id.clone());
                                    if let (Some(m), Some(ch)) =
                                        (idx.and_then(|i| state.messages.get(i)), ch)
                                    {
                                        state.mod_action_menu =
                                            Some(crate::app::ModActionMenu {
                                                channel: ch,
                                                ts: m.ts.clone(),
                                                cursor: 0,
                                            });
                                    }
                                }
                            }
                            #[cfg(feature = "mods")]
                            KeyCode::F(7) => {
                                // Open the mod-view picker.
                                if !state.mod_views.is_empty() {
                                    state.mod_view_menu = Some(crate::app::ModViewMenu { cursor: 0 });
                                }
                            }
                            KeyCode::Char('q') if state.input.is_empty() => break,
                            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => break,
                            KeyCode::Right => {
                                // The thread opens on the message you have
                                // scrolled to, not on the first in the channel
                                // that has one: with ↑/↓ you reach any
                                // message, and a thread can be started on each
                                // of them even if it has none yet.
                                let idx = state
                                    .messages
                                    .len()
                                    .checked_sub(1 + state.message_scroll);
                                if let Some(m) = idx.and_then(|i| state.messages.get(i)) {
                                    let ts =
                                        m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
                                    let ch = state.channels[state.selected_channel].id.clone();
                                    state.view = ViewMode::Thread;
                                    drop(state);
                                    app.open_thread(ch, ts);
                                    state = app.state().write().await;
                                }
                            }
                            KeyCode::Char(c) => state.input.push(c),
                            KeyCode::Enter if !state.input.is_empty() => {
                                let text = state.input.clone();
                                let channel_id = state.channels[state.selected_channel].id.clone();
                                let api = app.api().clone();
                                let state_clone = app.state().clone();
                                tokio::spawn(async move {
                                    match api.chat_post_message(&channel_id, &text).await {
                                        Ok(resp) => {
                                            let mut s = state_clone.write().await;
                                            s.messages.insert(0, MessageEntry {
                                                user: "me".into(),
                                                text: text.clone(),
                                                ts: resp.ts,
                                                reactions: Vec::new(),
                                                thread_ts: None,
                                                reply_count: None,
                                                author_icon: String::new(),
                                                attachments: Vec::new(),
                                    files: Vec::new(),
                                                actions: Vec::new(),
                                                bot_id: None,
                                                app_id: None,
                                            });
                                            s.input.clear();
                                            s.message_scroll = 0;
                                        }
                                        Err(e) => {
                                            let mut s = state_clone.write().await;
                                            s.status = t!("tui.send_error", e = e).to_string();
                                        }
                                    }
                                });
                            }
                            KeyCode::Up => {
                                if state.message_scroll + 8 < state.messages.len() {
                                    state.message_scroll += 1;
                                }
                            }
                            KeyCode::Down => {
                                if state.message_scroll > 0 {
                                    state.message_scroll -= 1;
                                }
                            }
                            KeyCode::Backspace => {
                                state.input.pop();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if state.view == ViewMode::Thread {
                        match key.code {
                            KeyCode::Esc => {
                                state.view = ViewMode::Chat;
                                state.input.clear();
                                state.message_scroll = 0;
                                state.status = t!("tui.messages", n = state.messages.len()).to_string();
                            }
                            KeyCode::Enter if !state.input.is_empty() => {
                                let text = state.input.clone();
                                let channel_id = state.channels[state.selected_channel].id.clone();
                                let thread_ts = state.thread_parent_ts.clone().unwrap_or_default();
                                let api = app.api().clone();
                                let state_clone = app.state().clone();
                                tokio::spawn(async move {
                                    match api.chat_post_message_thread(&channel_id, &text, &thread_ts).await {
                                        Ok(resp) => {
                                            let mut s = state_clone.write().await;
                                            s.thread_messages.push(MessageEntry {
                                                user: "me".into(),
                                                text: text.clone(),
                                                ts: resp.ts,
                                                reactions: Vec::new(),
                                                thread_ts: None,
                                                reply_count: None,
                                                author_icon: String::new(),
                                                attachments: Vec::new(),
                                    files: Vec::new(),
                                                actions: Vec::new(),
                                                bot_id: None,
                                                app_id: None,
                                            });
                                            s.input.clear();
                                        }
                                        Err(e) => {
                                            let mut s = state_clone.write().await;
                                            s.status = t!("tui.thread_reply_error", e = e).to_string();
                                        }
                                    }
                                });
                            }
                            KeyCode::Enter => {
                                state.view = ViewMode::Chat;
                                state.input.clear();
                                state.message_scroll = 0;
                                state.status = t!("tui.messages", n = state.messages.len()).to_string();
                            }
                            KeyCode::F(2) => app.open_huddle(&mut state),
                            KeyCode::Char('q') if state.input.is_empty() => break,
                            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => break,
                            KeyCode::Char(c) => state.input.push(c),
            KeyCode::Up => {
                if state.thread_scroll > 0 {
                    state.thread_scroll -= 1;
                }
            }
            KeyCode::Down => {
                if state.thread_scroll + 8 < state.thread_messages.len() {
                    state.thread_scroll += 1;
                }
            }
                            KeyCode::Backspace => {
                                state.input.pop();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if state.search_mode {
                        match key.code {
                            KeyCode::Esc => {
                                state.search_mode = false;
                                state.search_query.clear();
                                state.status = t!("tui.channels", n = state.channels.len()).to_string();
                            }
                            KeyCode::Enter => {
                                let q = state.search_query.to_lowercase();
                                let pos = state.channels.iter().position(|ch| ch.name.to_lowercase().contains(&q));
                                if let Some(idx) = pos {
                                    state.selected_channel = idx;
                                    App::adjust_channel_scroll(&mut state);
                                    let channel_id = state.channels[idx].id.clone();
                                    app.remember_channel(&channel_id);
                                    state.search_mode = false;
                                    state.search_query.clear();
                                    state.view = ViewMode::Chat;
                                    state.messages.clear();
                                    state.message_scroll = 0;
                                    state.input.clear();
                                    state.status = t!("tui.loading").to_string();
                                    drop(state);
                                    App::load_channel_messages(channel_id, &app.state(), &app.api(), &app.store());
                                } else {
                                    state.search_mode = false;
                                    state.search_query.clear();
                                    state.status = t!("tui.channels", n = state.channels.len()).to_string();
                                }
                            }
                            KeyCode::Backspace => {
                                state.search_query.pop();
                            }
                            KeyCode::Char(c) => {
                                state.search_query.push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::F(2) => app.open_huddle(&mut state),
                            KeyCode::Char('q') if state.input.is_empty() => break,
                        KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => break,
                        KeyCode::Char('/') => {
                            state.search_mode = true;
                            state.search_query.clear();
                            state.status = t!("tui.search_hint").to_string();
                        }
                        KeyCode::Enter if !state.channels.is_empty() => {
                            let channel_id = state.channels[state.selected_channel].id.clone();
                            app.remember_channel(&channel_id);
                            state.view = ViewMode::Chat;
                            state.messages.clear();
                            state.message_scroll = 0;
                            state.input.clear();
                            state.status = t!("tui.loading").to_string();
                            drop(state);
                            App::load_channel_messages(channel_id, &app.state(), &app.api(), &app.store());
                        }
                        KeyCode::Up => {
                            if state.selected_channel > 0 {
                                state.selected_channel -= 1;
                                App::adjust_channel_scroll(&mut state);
                            }
                        }
                        KeyCode::Down => {
                            if state.selected_channel + 1 < state.channels.len() {
                                state.selected_channel += 1;
                                App::adjust_channel_scroll(&mut state);
                            }
                        }
                        _ => {}
                    }
                }
                Some(update) = ws_rx.recv() => {
                    App::apply_ws_update(update, &app.state(), &app.store(), &app.api()).await;
                }
                _ = ticker.tick() => {
                    // A composer action may have asked the composer to hold text.
                    #[cfg(feature = "mods")]
                    {
                        let channel = {
                            let s = app.state().read().await;
                            s.channels.get(s.selected_channel).map(|c| c.id.clone())
                        };
                        if let Some(ch) = channel {
                            if let Some(text) = App::take_composer_prefill(&app.state(), &ch) {
                                app.state().write().await.input = text;
                            }
                        }
                    }
                    let read_state = app.state().read().await;
                    let _ = terminal.draw(|f| render(f, &read_state));
                }
            }
        }

        crossterm::terminal::disable_raw_mode()?;
        Ok(())
    }

fn render(frame: &mut Frame, state: &AppState) {
    match state.view {
        ViewMode::ChannelList => render_channel_list(frame, state),
        ViewMode::Chat => render_chat(frame, state),
        ViewMode::Thread => render_thread(frame, state),
    }
    #[cfg(feature = "mods")]
    if let Some(dialog) = &state.mods_consent {
        render_mods_consent(frame, dialog);
    }
    #[cfg(feature = "mods")]
    if let Some(dialog) = &state.mods_settings {
        render_mods_settings(frame, dialog);
    }
    #[cfg(feature = "mods")]
    if let Some(menu) = &state.mod_action_menu {
        // The TUI has no right-click: `ui.contextMenu` items ride the action menu.
        let actions: Vec<crate::app::ModAction> = state
            .mod_actions
            .iter()
            .chain(state.mod_context_actions.iter())
            .cloned()
            .collect();
        render_mods_action_menu(frame, menu, &actions);
    }
    #[cfg(feature = "mods")]
    if let Some(menu) = &state.mod_view_menu {
        render_mod_view_menu(frame, menu, &state.mod_views);
    }
    #[cfg(feature = "mods")]
    if let Some(panel) = &state.mod_view_panel {
        render_mod_view_panel(frame, panel);
    }
    #[cfg(feature = "mods")]
    if let Some(dialog) = &state.mod_dialog {
        render_mod_dialog(frame, dialog, state.mod_dialog_cursor);
    }
}

/// The number of capability rows across every mod in the dialog.
#[cfg(feature = "mods")]
fn consent_cap_count(d: &crate::app::ModConsentDialog) -> usize {
    d.entries.iter().map(|e| e.caps.len()).sum()
}

/// Toggles the capability under the cursor.
#[cfg(feature = "mods")]
fn toggle_consent_cap(d: &mut crate::app::ModConsentDialog) {
    let cur = d.cursor;
    let mut i = 0usize;
    for e in &mut d.entries {
        for c in &mut e.caps {
            if i == cur {
                c.selected = !c.selected;
                return;
            }
            i += 1;
        }
    }
}

/// What the settings form asked the caller to do after handling a key.
#[cfg(feature = "mods")]
enum SettingsAction {
    None,
    Apply,
    Dismiss,
}

/// The field under the cursor in the current tab.
#[cfg(feature = "mods")]
fn mod_settings_current(s: &mut AppState) -> Option<&mut crate::app::ModSettingField> {
    let d = s.mods_settings.as_mut()?;
    let (tab, cursor) = (d.tab, d.cursor);
    d.panes.get_mut(tab)?.fields.get_mut(cursor)
}

/// Cycles the mod tab (a wrap-around).
#[cfg(feature = "mods")]
fn mod_settings_cycle_tab(s: &mut AppState, delta: isize) {
    if let Some(d) = s.mods_settings.as_mut() {
        let n = d.panes.len();
        if n == 0 {
            return;
        }
        d.tab = (d.tab as isize + delta).rem_euclid(n as isize) as usize;
        d.cursor = 0;
        d.editing = false;
    }
}

/// Activates the focused field: a `bool`/`enum` changes value, a text-like field
/// enters (or, with `dir != 0`, keeps cycling an enum) inline editing.
#[cfg(feature = "mods")]
fn mod_settings_activate(s: &mut AppState, dir: isize) {
    use crate::mods::settings::Kind;
    let kind = match mod_settings_current(s) {
        Some(f) => f.kind,
        None => return,
    };
    match kind {
        Kind::Bool => {
            if let Some(f) = mod_settings_current(s) {
                f.text = if f.text == "true" { "false" } else { "true" }.to_string();
                f.has_value = true;
                f.dirty = true;
            }
        }
        Kind::Enum => {
            if let Some(f) = mod_settings_current(s) {
                if f.options.is_empty() {
                    return;
                }
                let n = f.options.len();
                let j = match f.options.iter().position(|o| *o == f.text) {
                    Some(i) => {
                        let step = if dir < 0 { -1 } else { 1 };
                        (i as isize + step).rem_euclid(n as isize) as usize
                    }
                    // Unset: forward → the first option, backward → the last.
                    None => {
                        if dir < 0 {
                            n - 1
                        } else {
                            0
                        }
                    }
                };
                f.text = f.options[j].clone();
                f.has_value = true;
                f.dirty = true;
            }
        }
        _ => {
            if let Some(d) = s.mods_settings.as_mut() {
                d.editing = true;
            }
        }
    }
}

/// Handles one key for the settings form. Navigation and text editing mutate the
/// (locked) state in place; Save/Close are returned for the caller to run after
/// dropping the lock.
#[cfg(feature = "mods")]
fn mod_settings_keys(s: &mut AppState, key: event::KeyEvent) -> SettingsAction {
    use crossterm::event::KeyCode;
    // A save in flight owns the form: ignore every key.
    if s.mods_settings.as_ref().map(|d| d.busy).unwrap_or(false) {
        return SettingsAction::None;
    }
    let editing = s.mods_settings.as_ref().map(|d| d.editing).unwrap_or(false);
    if editing {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                if let Some(d) = s.mods_settings.as_mut() {
                    d.editing = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = mod_settings_current(s) {
                    f.text.pop();
                    f.has_value = true;
                    f.dirty = true;
                    f.clear = false;
                }
            }
            KeyCode::Char(c) => {
                if let Some(f) = mod_settings_current(s) {
                    f.text.push(c);
                    f.has_value = true;
                    f.dirty = true;
                    f.clear = false;
                }
            }
            _ => {}
        }
        return SettingsAction::None;
    }
    match key.code {
        KeyCode::Esc => return SettingsAction::Dismiss,
        KeyCode::Char('a') => return SettingsAction::Apply,
        KeyCode::Up => {
            if let Some(d) = s.mods_settings.as_mut() {
                d.cursor = d.cursor.saturating_sub(1);
                d.editing = false;
            }
        }
        KeyCode::Down => {
            if let Some(d) = s.mods_settings.as_mut() {
                let n = d.panes.get(d.tab).map(|p| p.fields.len()).unwrap_or(0);
                if d.cursor + 1 < n {
                    d.cursor += 1;
                }
                d.editing = false;
            }
        }
        KeyCode::Tab => mod_settings_cycle_tab(s, 1),
        KeyCode::BackTab => mod_settings_cycle_tab(s, -1),
        KeyCode::Left => mod_settings_activate(s, -1),
        KeyCode::Right => mod_settings_activate(s, 1),
        KeyCode::Enter | KeyCode::Char(' ') => mod_settings_activate(s, 0),
        KeyCode::Delete => {
            // Clear a stored secret (the buffer holds only a replacement).
            if let Some(f) = mod_settings_current(s) {
                if f.kind.is_secret() && f.has_value {
                    f.text.clear();
                    f.has_value = false;
                    f.clear = true;
                    f.dirty = true;
                }
            }
        }
        _ => {}
    }
    SettingsAction::None
}

/// Draws the settings form: a tab per mod, a row per field (secrets masked).
#[cfg(feature = "mods")]
fn render_mods_settings(frame: &mut Frame, dialog: &crate::app::ModSettingsDialog) {
    use crate::mods::settings::Kind;
    use ratatui::widgets::Clear;
    let area = frame.area();
    let width = (area.width * 3 / 4).clamp(40.min(area.width), area.width);
    let height = (area.height * 3 / 4).clamp(6.min(area.height), area.height);
    let rect = Rect::new(
        area.width.saturating_sub(width) / 2,
        area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(t!("tui.mods_settings_title").to_string());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(if dialog.error.is_some() { 2 } else { 1 }),
        ])
        .split(inner);

    // One tab per mod.
    let mut tabs: Vec<Span> = Vec::new();
    for (i, p) in dialog.panes.iter().enumerate() {
        let style = if i == dialog.tab {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        };
        tabs.push(Span::styled(
            format!(" {} ", crate::mods::sanitize(&p.name)),
            style,
        ));
        tabs.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), chunks[0]);

    // The fields of the current tab.
    let mut lines: Vec<Line> = Vec::new();
    if let Some(pane) = dialog.panes.get(dialog.tab) {
        for (i, f) in pane.fields.iter().enumerate() {
            let value = match f.kind {
                Kind::Bool => {
                    if f.text == "true" {
                        "[x]".to_string()
                    } else {
                        "[ ]".to_string()
                    }
                }
                Kind::Secret => {
                    if i == dialog.cursor && dialog.editing {
                        // The buffer holds only a replacement the user is typing;
                        // the stored secret is never loaded into it.
                        f.text.clone()
                    } else if f.has_value {
                        "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}".to_string()
                    } else {
                        t!("tui.mods_settings_unset").to_string()
                    }
                }
                // The label/value can come from an untrusted manifest or a mod's
                // `settings.set`: strip control characters before the terminal.
                _ => crate::mods::sanitize(&f.text),
            };
            let text = format!("  {:<26} {}", crate::mods::sanitize(f.display_label()), value);
            let style = if i == dialog.cursor {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    let visible = chunks[1].height as usize;
    let scroll = if visible == 0 {
        0
    } else {
        dialog.cursor.saturating_sub(visible.saturating_sub(1))
    };
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll as u16, 0)),
        chunks[1],
    );

    // A save error (red) and the key hint.
    if let Some(err) = &dialog.error {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(chunks[2]);
        frame.render_widget(
            Paragraph::new(crate::mods::sanitize(err)).style(Style::default().fg(Color::Red)),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(t!("tui.mods_settings_hint").to_string())
                .style(Style::default().fg(Color::DarkGray)),
            rows[1],
        );
    } else {
        frame.render_widget(
            Paragraph::new(t!("tui.mods_settings_hint").to_string())
                .style(Style::default().fg(Color::DarkGray)),
            chunks[2],
        );
    }
}

/// A bounded description of a read scope, for the consent dialog (a manifest can
/// list many channels; keep the row to one line so the cursor scroll math holds).
#[cfg(feature = "mods")]
fn scope_text(scope: &crate::mods::capability::ReadScope) -> String {
    use crate::mods::capability::ReadScope;
    match scope {
        ReadScope::All => t!("tui.mods_scope_all").to_string(),
        ReadScope::Channels { channels } => {
            let mut head: Vec<String> = channels
                .iter()
                .take(3)
                .map(|c| crate::mods::sanitize(c))
                .collect();
            let rest = channels.len().saturating_sub(head.len());
            if rest > 0 {
                head.push(format!("+{rest}"));
            }
            head.join(", ")
        }
    }
}

/// The translated heading for a capability's consent group.
#[cfg(feature = "mods")]
fn group_label(cap: crate::mods::capability::Capability) -> String {
    match cap.group() {
        "read" => t!("tui.mods_group_read").to_string(),
        "storage" => t!("tui.mods_group_storage").to_string(),
        "interface" => t!("tui.mods_group_interface").to_string(),
        "network" => t!("tui.mods_group_network").to_string(),
        "filesystem" => t!("tui.mods_group_filesystem").to_string(),
        "commands" => t!("tui.mods_group_commands").to_string(),
        "write" => t!("tui.mods_group_write").to_string(),
        "background" => t!("tui.mods_group_background").to_string(),
        other => other.to_string(),
    }
}

/// A bounded rendering of an egress allow-list (first few hosts, then a count).
#[cfg(feature = "mods")]
fn hosts_text(hosts: &[String]) -> String {
    let mut head: Vec<String> = hosts
        .iter()
        .take(3)
        .map(|h| crate::mods::sanitize(h))
        .collect();
    let rest = hosts.len().saturating_sub(head.len());
    if rest > 0 {
        head.push(format!("+{rest}"));
    }
    head.join(", ")
}

/// Draws the consent dialog centred over the current view. Every line is
/// sanitized at build time (the names/ids come from untrusted manifests).
#[cfg(feature = "mods")]
fn render_mods_consent(frame: &mut Frame, dialog: &crate::app::ModConsentDialog) {
    use ratatui::widgets::Clear;
    let area = frame.area();
    let width = (area.width * 3 / 4).clamp(40.min(area.width), area.width);
    let height = (area.height * 3 / 4).clamp(6.min(area.height), area.height);
    let rect = Rect::new(
        area.width.saturating_sub(width) / 2,
        area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);

    // The hint sits at the bottom; the mods scroll above it.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(t!("tui.mods_title").to_string());
    let visible = block.inner(chunks[0]).height as usize;

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_line = 0usize;
    // The last line of the cursor's block (its `filesystem` slot lines follow
    // the row), so the scroll can bring the whole block into view.
    let mut cursor_block_end = 0usize;
    let mut ci = 0usize;
    for e in &dialog.entries {
        lines.push(Line::from(Span::styled(
            format!("{} ({})", e.name, e.id),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        // Capabilities are grouped under their consent heading (Read / Storage /
        // Interface); each stays its own toggle.
        let mut heading = "";
        for c in &e.caps {
            if c.cap.group() != heading {
                heading = c.cap.group();
                lines.push(Line::from(Span::styled(
                    format!("  {}", group_label(c.cap)),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
            let is_cursor = ci == dialog.cursor;
            if is_cursor {
                cursor_line = lines.len();
            }
            let mark = if c.selected { "[x]" } else { "[ ]" };
            let extra = match c.cap {
                crate::mods::capability::Capability::MessagesRead => {
                    match e.effective_scope.as_ref().or(e.read_scope.as_ref()) {
                        Some(scope) => format!(" \u{2014} {}", scope_text(scope)),
                        None => String::new(),
                    }
                }
                crate::mods::capability::Capability::NetEgress if !e.egress.is_empty() => {
                    format!(" \u{2014} {}", hosts_text(&e.egress))
                }
                _ => String::new(),
            };
            let text = format!("    {mark} {}{extra}", c.cap.name());
            let style = if ci == dialog.cursor {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if c.selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(text, style)));
            ci += 1;
            // `filesystem` slot mappings are shown read-only (the TUI has no
            // picker): one indented line per declared slot, under the row.
            if c.cap == crate::mods::capability::Capability::Filesystem {
                for slot in &e.fs_slots {
                    let mapped = slot
                        .mapped
                        .as_deref()
                        .map(crate::mods::sanitize)
                        .unwrap_or_else(|| t!("tui.mods_slot_unmapped").to_string());
                    lines.push(Line::from(Span::styled(
                        format!(
                            "      {} ({}) \u{2014} {}",
                            crate::mods::sanitize(&slot.name),
                            slot.access.as_str(),
                            mapped
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            if is_cursor {
                // The block ends after its `filesystem` slot lines (at the row
                // itself for every other capability): the scroll keeps the whole
                // block in view, not just the row.
                cursor_block_end = lines.len().saturating_sub(1);
            }
        }
        if e.read_and_network() {
            lines.push(Line::from(Span::styled(
                format!("  {}", t!("tui.mods_danger_read_network")),
                Style::default().fg(Color::Red),
            )));
        }
        if e.files_and_network() {
            lines.push(Line::from(Span::styled(
                format!("  {}", t!("tui.mods_danger_files_network")),
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::from(""));
    }

    // Keep the highlighted row on screen with a lot of installed mods, and bring
    // the whole of its block into view when it fits: a `filesystem` row is
    // followed by one line per declared slot, which must not sit below the fold.
    let scroll = if visible == 0 {
        0
    } else {
        let page = visible - 1;
        let block = cursor_block_end.saturating_sub(page);
        let row = cursor_line.saturating_sub(page);
        // Prefer showing the block's last line, but never scroll the highlighted
        // row off the top (when the block is taller than the dialog).
        block.max(row).min(cursor_line)
    };
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(paragraph, chunks[0]);

    let hint = Paragraph::new(t!("tui.mods_hint").to_string())
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, chunks[1]);
}

/// Draws the mod-action menu: the actions a mod declared, for one message.
#[cfg(feature = "mods")]
fn render_mods_action_menu(
    frame: &mut Frame,
    menu: &crate::app::ModActionMenu,
    actions: &[crate::app::ModAction],
) {
    use ratatui::widgets::Clear;
    let area = frame.area();
    let width = (area.width.saturating_mul(2) / 3).clamp(30.min(area.width), area.width);
    let height = (actions.len() as u16 + 3).clamp(3.min(area.height), area.height);
    let rect = Rect::new(
        area.width.saturating_sub(width) / 2,
        area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(t!("tui.mods_action_title").to_string());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let mut lines: Vec<Line> = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        let icon = a.icon.as_deref().filter(|i| !i.is_empty()).unwrap_or("");
        let text = format!(
            "  {icon}{}  \u{00b7} {}",
            crate::mods::sanitize(&a.label),
            crate::mods::sanitize(&a.mod_name)
        );
        let style = if i == menu.cursor {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines), chunks[0]);
    frame.render_widget(
        Paragraph::new(t!("tui.mods_action_hint").to_string())
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

/// Draws the mod-view picker: one row per declared view.
#[cfg(feature = "mods")]
fn render_mod_view_menu(
    frame: &mut Frame,
    menu: &crate::app::ModViewMenu,
    views: &[crate::app::ModView],
) {
    use ratatui::widgets::Clear;
    let area = frame.area();
    let width = (area.width.saturating_mul(2) / 3).clamp(30.min(area.width), area.width);
    let height = (views.len() as u16)
        .saturating_add(3)
        .clamp(3.min(area.height), area.height);
    let rect = Rect::new(
        area.width.saturating_sub(width) / 2,
        area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(t!("tui.mods_views_title").to_string());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let mut lines: Vec<Line> = Vec::new();
    for (i, v) in views.iter().enumerate() {
        let icon = v.icon.as_deref().filter(|i| !i.is_empty()).unwrap_or("");
        let text = format!(
            "  {icon}{}  \u{00b7} {}",
            crate::mods::sanitize(&v.label),
            crate::mods::sanitize(&v.mod_name)
        );
        let style = if i == menu.cursor {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines), chunks[0]);
    frame.render_widget(
        Paragraph::new(t!("tui.mods_views_hint").to_string())
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

/// Draws an open mod-view panel: its title and `(kind, text)` content blocks.
#[cfg(feature = "mods")]
fn render_mod_view_panel(frame: &mut Frame, panel: &crate::app::ModViewPanel) {
    use ratatui::widgets::Clear;
    let area = frame.area();
    let width = (area.width.saturating_mul(3) / 4).clamp(40.min(area.width), area.width);
    let height = (area.height.saturating_mul(3) / 4).clamp(6.min(area.height), area.height);
    let rect = Rect::new(
        area.width.saturating_sub(width) / 2,
        area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(crate::mods::sanitize(&panel.title));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let mut lines: Vec<Line> = Vec::new();
    for (kind, text) in panel.blocks.iter() {
        match kind.as_str() {
            "divider" => lines.push(Line::from(Span::styled(
                "\u{2500}".repeat(chunks[0].width as usize),
                Style::default().fg(Color::DarkGray),
            ))),
            "context" => lines.push(Line::from(Span::styled(
                format!("  {}", crate::mods::sanitize(text)),
                Style::default().fg(Color::DarkGray),
            ))),
            _ => lines.push(Line::from(format!("  {}", crate::mods::sanitize(text)))),
        }
    }
    if panel.blocks.is_empty() {
        lines.push(Line::from(Span::styled(
            t!("tui.mods_view_empty").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);
    frame.render_widget(
        Paragraph::new(t!("tui.mods_view_hint").to_string())
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

/// A mod dialog as a centered modal: the title, `(kind, text)` blocks and an
/// action per `(id, label)` followed by an always-available "Dismiss" row. The
/// highlighted row is `cursor` (one past the last action is Dismiss).
#[cfg(feature = "mods")]
fn render_mod_dialog(frame: &mut Frame, dialog: &crate::app::ModDialog, cursor: usize) {
    use ratatui::widgets::Clear;
    let area = frame.area();
    let width = (area.width.saturating_mul(2) / 3).clamp(40.min(area.width), area.width);
    let height = (area.height.saturating_mul(2) / 3).clamp(6.min(area.height), area.height);
    let rect = Rect::new(
        area.width.saturating_sub(width) / 2,
        area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(crate::mods::sanitize(&dialog.title));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    // The body and the action rows get their own areas: the buttons (and the
    // always-available Dismiss) are never pushed off by a long body, which is
    // clipped to its own area instead (so wrapping cannot defeat the reserve).
    let action_rows = (dialog.actions.len() + 1).min(chunks[0].height as usize) as u16;
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(action_rows)])
        .split(chunks[0]);

    let mut content: Vec<Line> = Vec::new();
    for (kind, text) in dialog.blocks.iter() {
        match kind.as_str() {
            "divider" => content.push(Line::from(Span::styled(
                "\u{2500}".repeat(chunks[0].width as usize),
                Style::default().fg(Color::DarkGray),
            ))),
            "context" => content.push(Line::from(Span::styled(
                format!("  {}", crate::mods::sanitize(text)),
                Style::default().fg(Color::DarkGray),
            ))),
            _ => content.push(Line::from(format!("  {}", crate::mods::sanitize(text)))),
        }
    }
    if dialog.blocks.is_empty() {
        content.push(Line::from(Span::styled(
            t!("tui.mods_dialog_empty").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    // Bound what the paragraph lays out (a trailing ellipsis marks a clipped body);
    // the paragraph also clips to its area, so a wrapped line is safe.
    let body_h = body[0].height as usize;
    if content.len() > body_h && body_h > 0 {
        content.truncate(body_h.saturating_sub(1));
        content.push(Line::from(Span::styled(
            "  \u{2026}",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(content).wrap(Wrap { trim: false }),
        body[0],
    );

    // The actions, then the always-available Dismiss; the cursor selects one.
    let mut lines: Vec<Line> = Vec::new();
    for (i, (_, label)) in dialog.actions.iter().enumerate() {
        let style = if i == cursor {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", crate::mods::sanitize(label)),
            style,
        )));
    }
    let dismiss = dialog.actions.len();
    let style = if dismiss == cursor {
        Style::default().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::default()
    };
    lines.push(Line::from(Span::styled(
        format!("  {}", t!("tui.mods_dialog_close").to_string()),
        style,
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body[1]);
    frame.render_widget(
        Paragraph::new(t!("tui.mods_dialog_hint").to_string())
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

fn render_channel_list(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    let filtered = filtered_channels(&state.channels, &state.search_query);
    let visible_height = (chunks[0].height as usize).saturating_sub(2);
    let scroll = if state.search_mode {
        0
    } else if state.search_query.is_empty() {
        state
            .channel_scroll
            .min(if filtered.is_empty() { 0 } else { filtered.len() - 1 })
    } else {
        0
    };
    let visible_channels: Vec<&ChannelEntry> = filtered
        .iter()
        .skip(scroll)
        .take(visible_height)
        .copied()
        .collect();

    let selected_id = state.channels.get(state.selected_channel).map(|c| &c.id);

    let channel_items: Vec<ListItem> = visible_channels
        .iter()
        .enumerate()
        .map(|(display_i, ch)| {
            let actual_idx = display_i + scroll;
            let is_selected = !state.search_mode
                && actual_idx < filtered.len()
                && selected_id == Some(&filtered[actual_idx].id);
            let style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!("#{}", ch.name),
                style,
            )]))
        })
        .collect();

    let channel_title = if state.search_mode {
        t!("tui.search_title").to_string()
    } else {
        t!("tui.channels_title").to_string()
    };
    let channel_list = List::new(channel_items)
        .block(Block::default().borders(Borders::ALL).title(channel_title));
    frame.render_widget(channel_list, chunks[0]);

    let help_lines = vec![
        Line::from(Span::styled(
            t!("tui.help_navigate").to_string(),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(
            t!("tui.help_open_chat").to_string(),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(
            t!("tui.help_search").to_string(),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(t!("tui.help_quit").to_string(), Style::default().fg(Color::Cyan))),
    ];
    let help =
        Paragraph::new(help_lines).block(Block::default().borders(Borders::ALL).title(t!("tui.help_title").to_string()));
    frame.render_widget(help, chunks[1]);

    let status_text = if state.search_mode {
        let matches = if state.search_query.is_empty() {
            state.channels.len()
        } else {
            let q = state.search_query.to_lowercase();
            state
                .channels
                .iter()
                .filter(|ch| ch.name.to_lowercase().contains(&q))
                .count()
        };
        t!(
            "tui.search_status",
            query = state.search_query.as_str(),
            matches = matches
        )
        .to_string()
    } else {
        state.status.clone()
    };
    // ● connected and listening, ○ disconnected: without this there is no
    // telling "no new messages" from "nothing is arriving any more".
    let status_text = format!(
        "{} {}",
        if state.live { "\u{25cf}" } else { "\u{25cb}" },
        status_text
    );
    let status = Paragraph::new(status_text.as_str()).style(
        Style::default().fg(if state.live {
            Color::Green
        } else {
            Color::DarkGray
        }),
    );
    frame.render_widget(
        status,
        Rect::new(0, area.height.saturating_sub(1), area.width, 1),
    );
}

fn render_chat(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let channel_name = state
        .channels
        .get(state.selected_channel)
        .map(|c| format!("#{}", c.name))
        .unwrap_or_else(|| t!("tui.chat").to_string());

    let content_height = (chunks[0].height as usize).saturating_sub(2);
    let widget_width = (chunks[0].width as usize).saturating_sub(2);

    let msg_lines = |msg: &MessageEntry| -> usize {
        let text_lines = std::cmp::max(1, (msg.text.len() + widget_width) / widget_width.max(1));
        1 + text_lines + msg.reactions.len() + 1
            + if msg.thread_ts.is_some() && msg.reply_count.unwrap_or(0) > 0 {
                1
            } else {
                0
            }
    };

    let msg_count = state.messages.len();
    let scroll_start = state.message_scroll.min(msg_count.saturating_sub(1));

    let mut used_lines = 0usize;
    let mut end = scroll_start;

    if scroll_start > 0 {
        used_lines += 1;
    }

    for i in scroll_start..msg_count {
        let lines = msg_lines(&state.messages[i]);
        if used_lines + lines + 1 <= content_height {
            used_lines += lines;
            end = i + 1;
        } else {
            break;
        }
    }

    let first_visible_idx = scroll_start;
    let last_visible_idx = if end > 0 { end - 1 } else { 0 };

    let mut message_lines: Vec<Line> = Vec::new();

    if first_visible_idx > 0 {
        message_lines.push(Line::from(Span::styled(
            t!("tui.older_messages_above").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    for msg in state.messages[first_visible_idx..end].iter().rev() {
        let display_name = App::user_name(state, &msg.user);
        let time = format_ts(&msg.ts);
        message_lines.push(Line::from(vec![
            Span::styled(
                format!(" {}  ", time),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                display_name,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        message_lines.push(Line::from(Span::raw(format!("  {}", msg.text))));

        // Mod decorations for this message (host-set, sanitized).
        #[cfg(feature = "mods")]
        if let Some(deco) = state
            .channels
            .get(state.selected_channel)
            .and_then(|c| state.decorations.get(&c.id))
            .and_then(|by_ts| by_ts.get(&msg.ts))
        {
            for d in deco {
                message_lines.push(Line::from(Span::styled(
                    format!("    {}", crate::mods::sanitize(d)),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
        }

        for r in &msg.reactions {
            message_lines.push(Line::from(vec![Span::styled(
                format!("    :{}: {}", r.name, r.count),
                Style::default().fg(Color::Magenta),
            )]));
        }
        if let Some(ref _thread_ts) = msg.thread_ts {
            let count = msg.reply_count.unwrap_or(0);
            if count > 0 {
                message_lines.push(Line::from(vec![Span::styled(
                    t!("tui.replies", n = count).to_string(),
                    Style::default().fg(Color::Blue),
                )]));
            }
        }
        message_lines.push(Line::from(Span::styled(
            "\u{2500}".repeat(area.width as usize),
            Style::default().fg(Color::DarkGray),
        )));
    }

    if first_visible_idx == 0 {
        message_lines.push(Line::from(Span::styled(
            t!("tui.newest_messages").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        message_lines.push(Line::from(Span::styled(
            t!("tui.more_recent_below").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let messages = Paragraph::new(message_lines)
        .block(Block::default().borders(Borders::ALL).title(channel_name))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);

    let input = Paragraph::new(state.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(t!("tui.input_title").to_string()));
    frame.render_widget(input, chunks[1]);

    let status_text = if state.messages.is_empty() {
        state.status.clone()
    } else {
        t!(
            "tui.chat_status",
            status = state.status.as_str(),
            from = state.messages.len() - last_visible_idx,
            to = state.messages.len() - first_visible_idx,
            total = state.messages.len()
        )
        .to_string()
    };
    // ● connected and listening, ○ disconnected: without this there is no
    // telling "no new messages" from "nothing is arriving any more".
    let status_text = format!(
        "{} {}",
        if state.live { "\u{25cf}" } else { "\u{25cb}" },
        status_text
    );
    let status = Paragraph::new(status_text.as_str()).style(
        Style::default().fg(if state.live {
            Color::Green
        } else {
            Color::DarkGray
        }),
    );
    frame.render_widget(
        status,
        Rect::new(0, area.height.saturating_sub(1), area.width, 1),
    );
}

fn render_thread(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let channel_name = state
        .channels
        .get(state.selected_channel)
        .map(|c| t!("tui.thread_header", channel = c.name.as_str()).to_string())
        .unwrap_or_else(|| t!("tui.thread_title").to_string());

    let content_height = (chunks[0].height as usize).saturating_sub(2);
    let widget_width = (chunks[0].width as usize).saturating_sub(2);

    let msg_lines = |msg: &MessageEntry| -> usize {
        let text_lines = std::cmp::max(1, (msg.text.len() + widget_width) / widget_width.max(1));
        1 + text_lines + msg.reactions.len() + 1
            + if msg.thread_ts.is_some() && msg.reply_count.unwrap_or(0) > 0 {
                1
            } else {
                0
            }
    };

    let msg_count = state.thread_messages.len();
    let scroll_start = state.thread_scroll.min(msg_count.saturating_sub(1));

    let mut used_lines = 0usize;
    let mut end = scroll_start;

    if scroll_start > 0 {
        used_lines += 1;
    }

    for i in scroll_start..msg_count {
        let lines = msg_lines(&state.thread_messages[i]);
        if used_lines + lines + 1 <= content_height {
            used_lines += lines;
            end = i + 1;
        } else {
            break;
        }
    }

    let first_visible_idx = scroll_start;
    let last_visible_idx = if end > 0 { end - 1 } else { 0 };

    let mut message_lines: Vec<Line> = Vec::new();

    if first_visible_idx > 0 {
        message_lines.push(Line::from(Span::styled(
            t!("tui.older_messages_above").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    for (i, msg) in state.thread_messages[first_visible_idx..end].iter().enumerate() {
        let is_parent = (first_visible_idx + i) == 0;
        let display_name = App::user_name(state, &msg.user);
        let time = format_ts(&msg.ts);
        let name_style = if is_parent {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };
        message_lines.push(Line::from(vec![
            Span::styled(format!(" {}  ", time), Style::default().fg(Color::DarkGray)),
            Span::styled(display_name, name_style),
        ]));
        message_lines.push(Line::from(Span::raw(format!("  {}", msg.text))));

        for r in &msg.reactions {
            message_lines.push(Line::from(vec![Span::styled(
                format!("    :{}: {}", r.name, r.count),
                Style::default().fg(Color::Magenta),
            )]));
        }
        if let Some(ref _thread_ts) = msg.thread_ts {
            let count = msg.reply_count.unwrap_or(0);
            if count > 0 {
                message_lines.push(Line::from(vec![Span::styled(
                    t!("tui.replies", n = count).to_string(),
                    Style::default().fg(Color::Blue),
                )]));
            }
        }
        message_lines.push(Line::from(Span::styled(
            if is_parent {
                "\u{2501}".repeat(area.width as usize)
            } else {
                "\u{2500}".repeat(area.width as usize)
            },
            Style::default().fg(if is_parent { Color::Yellow } else { Color::DarkGray }),
        )));
    }

    if end == msg_count {
        message_lines.push(Line::from(Span::styled(
            t!("tui.newest_replies").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        message_lines.push(Line::from(Span::styled(
            t!("tui.more_recent_below").to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let thread_block = Block::default()
        .borders(Borders::ALL)
        .title(channel_name)
        .title_alignment(ratatui::layout::Alignment::Center);
    let messages = Paragraph::new(message_lines)
        .block(thread_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);

    let input = Paragraph::new(state.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(t!("tui.reply_title").to_string()));
    frame.render_widget(input, chunks[1]);

    let status_text = if state.thread_messages.is_empty() {
        state.status.clone()
    } else {
        t!(
            "tui.thread_status",
            status = state.status.as_str(),
            from = state.thread_messages.len() - last_visible_idx,
            to = state.thread_messages.len() - first_visible_idx,
            total = state.thread_messages.len()
        )
        .to_string()
    };
    // ● connected and listening, ○ disconnected: without this there is no
    // telling "no new messages" from "nothing is arriving any more".
    let status_text = format!(
        "{} {}",
        if state.live { "\u{25cf}" } else { "\u{25cb}" },
        status_text
    );
    let status = Paragraph::new(status_text.as_str()).style(
        Style::default().fg(if state.live {
            Color::Green
        } else {
            Color::DarkGray
        }),
    );
    frame.render_widget(
        status,
        Rect::new(0, area.height.saturating_sub(1), area.width, 1),
    );
}

#[cfg(all(test, feature = "mods"))]
mod mod_dialog_render_tests {
    use super::render_mod_dialog;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The rendered frame as text (one line per row).
    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            s.push('\n');
        }
        s
    }

    /// A body far taller than the dialog still leaves the action rows — and the
    /// always-available Dismiss — on screen (the layout reserves them).
    #[test]
    fn a_long_body_does_not_hide_the_actions() {
        let dialog = crate::app::ModDialog {
            mod_id: "dev.slaq.test".into(),
            title: "Test".into(),
            blocks: std::sync::Arc::new(
                (0..60)
                    .map(|i| ("section".to_string(), format!("line {i}")))
                    .collect(),
            ),
            actions: std::sync::Arc::new(vec![("ok".to_string(), "OKAY".to_string())]),
        };
        let mut term = Terminal::new(TestBackend::new(50, 16)).unwrap();
        term.draw(|f| render_mod_dialog(f, &dialog, 0)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("OKAY"), "the action row is visible:\n{text}");
        assert!(
            text.contains(&rust_i18n::t!("tui.mods_dialog_close").to_string()),
            "the Dismiss row is visible:\n{text}"
        );
    }
}

#[cfg(all(test, feature = "mods"))]
mod mods_consent_render_tests {
    use super::render_mods_consent;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The rendered frame as text (one line per row).
    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            s.push('\n');
        }
        s
    }

    /// With the cursor on a `filesystem` row, its slot lines must be scrolled
    /// into view, not left below the fold (the row is followed by one line per
    /// declared slot, unlike every other capability).
    #[test]
    fn a_filesystem_rows_slots_stay_visible() {
        use crate::app::{ModConsentCap, ModConsentDialog, ModConsentEntry, ModConsentSlot};
        use crate::mods::capability::Capability;
        use crate::mods::fs::FsAccess;
        let cap = |cap, selected| ModConsentCap {
            cap,
            selected,
            initial: selected,
        };
        let entry = ModConsentEntry {
            id: "dev.slaq.test".into(),
            name: "Test mod".into(),
            caps: vec![
                cap(Capability::MessagesRead, true),
                cap(Capability::StoreKv, false),
                cap(Capability::StoreVector, false),
                cap(Capability::StoreSql, false),
                cap(Capability::Filesystem, true),
            ],
            read_scope: None,
            effective_scope: None,
            egress: Vec::new(),
            access: Default::default(),
            fs_slots: vec![
                ModConsentSlot {
                    name: "inbox".into(),
                    access: FsAccess::Read,
                    mapped: Some("/home/user/docs".into()),
                },
                ModConsentSlot {
                    name: "outbox".into(),
                    access: FsAccess::Write,
                    mapped: None,
                },
            ],
        };
        // A small terminal so the list overflows and must scroll to the cursor.
        let dialog = ModConsentDialog {
            entries: vec![entry],
            pending: true,
            cursor: 4, // the `filesystem` row
        };
        let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
        term.draw(|f| render_mods_consent(f, &dialog)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("filesystem"), "the row is in view:\n{text}");
        assert!(
            text.contains("inbox (read)"),
            "the mapped slot line is in view:\n{text}"
        );
        assert!(
            text.contains("outbox (write)"),
            "the unmapped slot line is in view:\n{text}"
        );
    }
}
