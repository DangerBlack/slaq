#![no_main]
use libfuzzer_sys::fuzz_target;

// Slack's mrkdwn parser, compiled straight from the app's source so the fuzz
// build does not pull in the whole crate (GUI, libvpx, WebRTC, `screenshots`).
//
// `mrkdwn.rs` calls a couple of helpers in its parent module and the i18n macro
// for the `<!date^…>` expansion; the shim below provides them. Fuzzing only
// needs them to exist, not to be correct.
macro_rules! t {
    ($($tt:tt)*) => {
        String::new()
    };
}
fn day_name(_i: usize) -> String {
    String::new()
}
fn month_name(_i: usize) -> String {
    String::new()
}

#[path = "../../src/app/mrkdwn.rs"]
mod mrkdwn;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = mrkdwn::parse(&text);
    let _ = mrkdwn::parse_spans(&text);
    let _ = mrkdwn::to_plain(&text);
    let _ = mrkdwn::to_editable(&text);
});
