#![no_main]
use libfuzzer_sys::fuzz_target;
use prost::Message as _;

// The Chime signaling frame decoded off the WebSocket: one tag byte followed by
// a protobuf `SdkSignalFrame`. Both come from the server and are untrusted; a
// panic in the decode would close the client. The proto is regenerated here so
// the target does not depend on the whole `slaq` crate.
mod chime {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

fuzz_target!(|data: &[u8]| {
    let Some((_tag, payload)) = data.split_first() else {
        return;
    };
    let _ = chime::SdkSignalFrame::decode(payload);
});
