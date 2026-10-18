//! The light theme's component: it does nothing.
//!
//! A theme is pure data — the `ui.theme` table in `manifest.json`, which the
//! host merges onto its palette at load. slaq still instantiates a component
//! for every mod, so this one exports the guest interface and returns at once.
//! It imports nothing from the host.

#[allow(warnings)]
mod bindings;

use bindings::exports::slaq::api::guest::Guest;

struct Component;

impl Guest for Component {
    fn start() -> Result<(), String> {
        Ok(())
    }

    fn on_message(_channel: String, _ts: String, _author: String, _text: String) -> Result<(), String> {
        Ok(())
    }

    fn on_action(_id: String, _channel: String, _ts: String) -> Result<(), String> {
        Ok(())
    }

    fn on_event(_kind: String, _channel: String) -> Result<(), String> {
        Ok(())
    }

    fn on_reaction(
        _channel: String,
        _ts: String,
        _name: String,
        _user: String,
        _added: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    fn on_command(_name: String, _args: String, _channel: String) -> Result<(), String> {
        Ok(())
    }

    fn on_composer_action(_id: String, _channel: String, _draft: String) -> Result<(), String> {
        Ok(())
    }

    fn on_dialog_action(_id: String) -> Result<(), String> {
        Ok(())
    }
}

bindings::export!(Component with_types_in bindings);
