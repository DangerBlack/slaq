# Slack huddle protocol

Derived on 2026-09-04 from the webapp bundles of `a.slack-edge.com` (build `bv1-13-br`,
desktop client 4.52.155). Everything that follows is **read from the official client's
code**, not inferred; the few inferences are marked as such.

## WORKS (2026-09-30): two-party call verified

Rust client in huddle, second participant from official Slack on the same
channel:

```
map: stream=1 → 5b0c9f30…     the Rust client
map: stream=2 → 4d093803…     official Slack
live and varying volumes on both (35-59)
45 s session, signaling never dropped, 6772 packets received
```

Audio audible in both directions. The echo you hear when putting the two clients
on the same machine is expected (two microphones and two speakers in the same
room with no echo cancellation) and doesn't concern the call.

It was not necessary to change `receiveStreamIds`: the SFU sends the other
participants' stream anyway.

### In use since 2026-09-30

First real work call sustained by the client, without headphones.

Facts: interactive command (`slack call -i`), hooking into the GUI (`F2`, with a
bar for duration, mute and hang up), clean exit, echo and noise cancellation with
`sonora`, a pure-Rust port of WebRTC's audio module, with no C++ dependencies, so
the binary stays portable.

Remaining:

- **screen sharing**: separate Chime protocol, never analyzed; the URLs
  (`ScreenSharingUrl`, `ScreenViewingUrl`, `ScreenDataUrl`) already arrive in
  every invite;
- **incoming huddle**: `huddle_invite`, `sh_room_join` and `sh_room_update` are
  received but nobody handles them: an incoming call is invisible;
- **two cancellation parameters** never tuned: the delay estimate
  (`set_stream_delay_ms`) and the suppression aggressiveness.

---

## SOLVED (2026-09-30): it was the SRTP profile

**We offered `AEAD_AES_128_GCM` as the first profile. The SFU chose it, and our
packets weren't decryptable for it.** They left (558 in ten seconds, measured on
the wire), reached the TURN, and were silently discarded: no volume reported for
our stream, no response, and signaling teardown a few seconds later.

The official client offers `AES128_CM_SHA1_80` first. Offering only that:

```
30 seconds session, signaling never dropped, media always connected
AUDIO_METADATA: stream=1 volume=57..59   ← l'SFU ci sente
```

A comment in the code already suspected a problem with GCM, but it had removed
only the 256-bit variant, leaving the 128-bit one **in first position**, that is,
exactly the one that got negotiated.

How the diagnosis was reached: `tcpdump` capture of an official huddle and of one
of our probes, extraction of the DTLS `ClientHello` from the TURN encapsulation
(`capture/dtls.py`) and comparison of the `use_srtp` extension.

The sections that follow remain as the history of the journey; §9 and §10 contain
diagnoses later found to be wrong, flagged as such.

---

## The result in one line

Huddles aren't a Slack protocol: they're **Amazon Chime SDK**. Slack only acts as
authenticator and returns the credentials of a Chime meeting.

The bundle `gantry-v2-async-amazon-chime-sdk-js-experimental` is
`amazon-chime-sdk-js` **15.1.0**, Apache-2.0, public sources. The media plane and
the signaling frame format are therefore already documented: there's nothing to
reverse there. The only proprietary part is how to obtain the credentials.

## 1. Obtaining the meeting: `rooms.join`

Not `screenhero.rooms.create` (the one the code used is from another era). The
current family is `rooms.*`:

| Method               | Arguments                                          |
|----------------------|----------------------------------------------------|
| `rooms.join`         | `id?`, `channel_id?`, `thread_ts?`, `regions`, `multidevice` |
| `rooms.leave`        | `channel_id`, `call_id`, `attendee_id`, `reason`   |
| `rooms.notifyMember` | `channel_id`, `user_id`                            |
| `rooms.getLink`      | —                                                  |
| `screenhero.rooms.info` | `room`                                          |

Still around are `screenhero.rooms.{invite,refreshToken,rename,userEvent,proxyLogs,
proxyAnalytics,proxyMeetingEvents}`, ancillary.

`regions` comes from `getNearestChimeMediaRegion(teamId)`; if `shouldFetchRegion` is
false the client sends an empty string, so **it's optional**.

## 2. The response

The client passes it to `handleCallAPIResponse` (module `goQ3`, export `YY`), which
chooses between two backends:

```js
function handleCallAPIResponse(e) {
  return e.free_willy
    ? { meeting: e.free_willy.meeting, attendee: e.free_willy.attendee }  // Chime
    : { token, tokenExpiry: e.token_expiry, hostname: e.hostname,         // legacy
        port: e.port,
        turn: { username: e.turn_auth.username,
                password: e.turn_auth.password,
                ports:    e.turn_ports } };
}
```

`free_willy` is the internal codename of the Chime backend. The legacy branch is the
old ScreenHero: direct TURN, no SFU. **The Chime branch is the one to implement**;
the other only serves to know it can arrive and must be rejected with a clear error.

`call.free_willy.meeting` and `.attendee` are literally the AWS `CreateMeeting` /
`CreateAttendee` responses.

## 3. From meeting to connection

`MeetingSessionConfiguration` normalizes the keys to lowercase and reads:

```
meeting.meetingid
meeting.externalmeetingid
meeting.mediaplacement.audiohosturl
meeting.mediaplacement.signalingurl
meeting.mediaplacement.turncontrolurl
meeting.mediaplacement.eventingestionurl   (opzionale)
attendee.attendeeid
attendee.joinToken
```

The signaling WebSocket opens like this (`SignalingClientConnectionRequest`):

```
URL           <signalingurl>?X-Chime-Control-Protocol-Version=3&X-Amzn-Chime-Send-Close-On-Error=1
Sottoprotocolli  ["_aws_wt_session", "<joinToken>"]
```

The `joinToken` travels **as a WebSocket subprotocol**, not as a header nor a query
param. It's the easiest detail to get wrong.

### Framing

Every message is **a type byte followed by the protobuf**, not naked protobuf:

```
[0x05] [ SdkSignalFrame serializzato ]
```

`0x05` is `DefaultSignalingClient.FRAME_TYPE_RTC`. On receive the official client
also accepts `0x02` and on the other values it just emits a warning.

### The protobuf schema

The `amazon-chime-sdk-js` repo **doesn't version the `.proto`**, only the JS generated
by protobufjs. The `encode` functions however contain everything: field number, wire
type, name and, from the presence of the `if (message.x != null)` guard, whether the
field is `required` or `optional` (the schema is proto2).

`proto/reconstruct.py` reconstructs it: 53 messages, 13 enums, 225 fields, in
`proto/SignalingProtocol.proto`. The build uses `protox` (a pure-Rust protobuf
compiler) so `protoc` doesn't need to be installed.

Note: fields with enum type come out as `int32`. It's correct on the wire and also
more robust: a server introducing a new value doesn't make the decode fail.

### The JOIN frame

What the web client sends:

```
protocol_version = 2
flags            = SdkJoinFlags.HAS_STREAM_UPDATE  (2)
client_details   = { platform_name, platform_version, client_source,
                     chime_sdk_version, client_utc_offset, ... }
audio_session_id = <random>
wants_compressed_sdp = true       // opzionale: rifiutarla evita zlib
```

The server sends `PING_PONG` every 10 seconds and expects the pong.

## 4. Other useful crumbs

- Call ID: prefix `R`. Room ID: `Vr` or `zt-`.
- Message subtypes: `sh_room_created`, `sh_room_shared`.
- Benign errors on `rooms.leave`: `channel_not_found`, `room_not_found`,
  `invalid_channel_id`, `attendee_not_found`.

## 5. Verified in the field (2026-09-04, channel `C00000000`)

**`rooms.join` accepts an `xoxc` token.** It was the risk that could sink the
project: it didn't materialize. Real response, with `ok: true`:

```jsonc
{
  "call": {
    "call_id": "R000000000",
    "free_willy": {
      "attendee": {
        "AttendeeId": "0ea2843f-...",
        "ExternalUserId": "<team>-<call_id>-<user_id>",
        "JoinToken": "<98 characters>",
        "Capabilities": { "Audio": "SendReceive", "Video": "SendReceive",
                          "Content": "SendReceive" }
      },
      "meeting": {
        "MeetingId": "dedc1557-...",
        "ExternalMeetingId": "<team>-<call_id>-<user_id>",
        "MediaRegion": "eu-central-1",
        "MeetingArn": "arn:aws:chime:us-east-1:503555533201:meeting/<uuid>",
        "MediaPlacement": {
          "SignalingUrl":     "wss://signal.z3.ec1.m.chime.aws/control/<uuid>",
          "AudioHostUrl":     "<hash>.k.m3.ec1.app.chime.aws:3478",
          "AudioFallbackUrl": "wss://wss.k.m3.ec1.app.chime.aws:443/calls/<uuid>",
          "TurnControlUrl":   "https://<n>.cell.us-east-1.meetings.chime.aws/v2/turn_sessions",
          "ScreenSharingUrl": "wss://bitpw.m3.ec1.app.chime.aws:443/v2/screen/<uuid>",
          "ScreenDataUrl":    "wss://bitpw.m3.ec1.app.chime.aws:443/v2/screen/<uuid>",
          "ScreenViewingUrl": "wss://bitpw.m3.ec1.app.chime.aws:443/ws/connect?...",
          "EventIngestionUrl":"https://data.svc.ue1.ingest.chime.aws/v1/client-events"
        },
        "is_prewarmed": true
      }
    }
  },
  "huddle": { "id": "R000000000", "channels": ["C..."], "has_ended": false,
              "huddle_link": "https://app.slack.com/huddle/<team>/<channel>",
              "participants": [], "thread_root_ts": "..." },
  "canvas": { "root_thread_ts": "...", "thread_channel_id": "C..." }
}
```

Notes on what was learned by trying:

- **`regions` can be empty.** The `MediaRegion` field still comes back
  correct (`eu-central-1`), so Slack resolves it server-side.
- **The room is *prewarmed*.** `rooms.join` allocates the Chime meeting but **does
  not** enroll anyone: `participants: []` and `participants_events[<user>].joined =
  false`. You become a participant only by connecting to the SFU.
- **Repeated calls return the same room.** Two consecutive invocations gave the
  same `call_id` and the same `MeetingId`.
- **`rooms.leave` fails with `invalid_arguments`** after a `rooms.join` without a
  media connection, with every combination tried (`channel_id`+`call_id`,
  with and without `attendee_id`, with `id`, with `room`). The hypothesis is that
  it wants an attendee session that never existed. **Not verified**: the exact
  arguments remain unknown and shouldn't be guessed.
- **A `huddle_thread` message appears in the channel** (author `USLACKBOT`) already
  at `rooms.join`, with `has_ended: false`. With no participants it doesn't look
  like a huddle someone joined, but the trace in the channel is there.
- **The room expires by itself in about two minutes** if nobody connects:
  `date_start 12:37:40` → `date_end 12:39:46`, `has_ended: true`,
  `was_missed: false`. So `rooms.leave` isn't needed after a `rooms.join` without
  media: just do nothing. It's consistent with the hypothesis that `invalid_arguments`
  means "no attendee session to leave".

### Full chain verified (2026-09-04)

`rooms.join` → signaling WebSocket → `JOIN` → `JOIN_ACK`, all succeeded on the
first try with an `xoxc` token. Contents of the `JOIN_ACK`:

```
turn_credentials.uris     ["turn:ice.z2.ec1.m.chime.aws:3478?transport=udp",
                           "turns:ice.z2.ec1.m.chime.aws:443?transport=tcp"]
turn_credentials.username "1788529751:b5fce711-a866-11f1-aff7-06d73b415a95"
                          (unix_ts_di_scadenza:uuid)
turn_credentials.ttl      300
video_subscription_limit  25
```

The TURN servers therefore arrive **from the JOIN_ACK**, not from `TurnControlUrl`:
the `RTCConfiguration` must be built from here. The 300 s TTL implies that a long
session must renew them.

**A Chime JOIN doesn't make you a Slack participant.** After JOIN + JOIN_ACK the
room was still `participants: []`, `participant_history: []`,
`participants_events[<user>].joined = false`, and `has_ended: false`. The Slack-side
state evidently updates later, presumably when media actually flows (SUBSCRIBE with
SDP offer), or through a separate Slack call. This needs to be understood before one
can say to "be in a huddle".

Practical consequence: even the probe with `--connect` stays low-impact, because
nobody sees a participant in the channel.

Still to verify: what makes the room go to `joined: true` on the Slack side; whether
`multidevice: true` changes the response; which arguments `rooms.leave` wants from
inside a real media session.

## 6. Implementation in Rust

1. `rooms.join` with the existing `api::Client`: it's a normal form POST.
2. WebSocket to `signalingurl` with the subprotocol: `tokio-tungstenite` supports it.
3. `SdkSignalFrame` with `prost`, generating from the upstream `.proto`.
4. SDP from the frames → `webrtc` crate toward the SFU. Opus audio: `cpal` + `opus`
   are already among the dependencies and `calls/audio.rs` already does the capture.

Realistically: audio only. Video makes no sense in a terminal, and screen share can
be added later because it's a second transceiver on the same session.

## 7. Media session (2026-09-04)

### Works

Full chain up to the connection with the SFU:

```
rooms.join → signaling → JOIN/JOIN_ACK → SDP offer → SUBSCRIBE
           → SUBSCRIBE_ACK (sdp_answer) → DTLS-SRTP → connected
```

**Connecting media registers you as a Slack participant.** Querying the channel
while the PeerConnection was active: `participants: ['U00000000']`,
`participant_history: ['U00000000']`. It answers the question left open in §5: no
additional Slack call is needed, media alone does it.

The `SUBSCRIBE` for audio only, as the official client sends it:

```
duplex        = SdkStreamServiceType.RX     // RX even when sending audio: it describes the videos
sdp_offer     = <full offer, ICE already gathered — no trickle>
audio_host    = MediaPlacement.AudioHostUrl
audio_checkin = false
audio_muted   = <bool>
send_streams  = [ SdkStreamDescriptor {
                    media_type = AUDIO, track_label = "AmazonChimeExpressAudio",
                    attendee_id = <ours>, stream_id = 1, group_id = 1,
                    framerate = 15, max_bitrate_kbps = 600, avg_bitrate_bps = 400000 } ]
```

(`framerate` on an audio stream makes no sense, but the server expects it.)

### Two bugs found along the way

**`webrtc-srtp` 0.14.0 panics on SRTP_AEAD_AES_256_GCM.**
`ProtectionProfile::AeadAes256Gcm` declares `key_len() == 32`, but
`CipherAeadAesGcm` instantiates a fixed `Aes128Gcm` for both ciphers: the
32-byte key ends up in a cipher that wants 16 and the DTLS handshake aborts
inside `generic-array` (`assertion left == right failed, left: 32, right: 16`).
Chime negotiates that profile willingly. Workaround: exclude it with
`SettingEngine::set_srtp_protection_profiles`, the server falls back to AES-128.

**Our `calls/audio.rs` wasn't encoding in Opus.** The comment said it did, the
track was declared `audio/opus`, but it wrote raw PCM. Fixed with a real
`opus::Encoder`, and when muted it sends encoded silence instead of interrupting
the RTP stream.

### Open: signaling drops ~3 s after connect

With media active the signaling WebSocket is reset by the server
(`Connection reset without closing handshake`) about three seconds after the
PeerConnection goes to `connected`.

What was ruled out, with evidence:

- **It's not the keepalive.** With signaling alone (`--connect`, no media) the
  connection holds for a full 45 s sending a PING every 10 s.
- **It's not the first ping's delay**: the reset arrives at ~3 s, before the
  first ping is due.
- **It's not the missing data channel**: adding an `m=application` section to the
  offer changes nothing.
- **It's not Opus**: the reset stays identical after fixing the encoding.
- **It's not a reported protocol error**: removing
  `X-Amzn-Chime-Send-Close-On-Error` the server sends no error frame, it just
  closes.

So it's something in the media path that the SFU rejects without saying so. Leads
not yet tried: comparing the SDP webrtc-rs generates with the official client's
(capturable from `chrome://webrtc-internals`); periodic frames the client sends
after the SUBSCRIBE_ACK (`CLIENT_METRIC`, `AUDIO_STREAM_ID_INFO`); SDP attributes
expected by Chime and missing in our offer.

## 8. SDP comparison with the official client (2026-09-04)

Web client offer captured from `chrome://webrtc-internals`
(`setLocalDescription`). Watch out for the trap: the huddle preview screen turns on
mic and webcam, so `getUserMedia` appears **before** a PeerConnection exists. If you
save the dump there, `PeerConnections` is empty. You have to actually enter the
huddle.

### Differences found

| | official | ours (before) |
|---|---|---|
| `extmap ssrc-audio-level` | yes | absent |
| `extmap abs-send-time` | yes | absent |
| `extmap transport-wide-cc` | yes | absent |
| `extmap sdes:mid` | yes | absent |
| `rtcp-fb:111 transport-cc` | yes | absent |
| `rtcp-xr:rcvr-rtt=all` + `rrtr` | yes | absent |
| audio codec | red/63 + opus + G722/PCMU/PCMA/CN/telephone-event | opus + G722/PCMU/PCMA |
| m-line | audio + video (`a=inactive`), BUNDLE 0 1 | audio only |
| `ice-options:trickle` | yes | no (full offer) |

Added extmap and `transport-cc`. **The SFU accepts only `abs-send-time` and
`transport-wide-cc`**: in the response `sdes:mid` and `ssrc-audio-level` don't
appear. So the theory that the MID was missing for BUNDLE demux is wrong: Chime
doesn't use that MID.

Also added an `m=video`: webrtc-rs's `add_transceiver_from_kind` accepts only
`recvonly` (not `inactive`), which is anyway what's needed to receive others'
video.

### webrtc-rs limit on TURN

```
Unable to handle URL in gather_candidates_relay turns:ice.…:443?transport=tcp
could not get server reflexive address udp4 turns:…:443?transport=tcp: deadline has elapsed
```

webrtc-rs doesn't handle TURN over TLS/TCP: it uses only the UDP URI, and wastes
~5 seconds timing out on the other before completing ICE gathering. It must be
filtered upstream, passing only the `turn:…?transport=udp` URIs to the
`RTCConfiguration`.

### An error in the test bench

The wait loop had ended **after** `session.close()`: during the "hold" the
PeerConnection was already closed, and the signaling reset at ~3 s was the SFU
reacting to the media disappearing. Fixed (media now stays open and its state is
sampled every second), but with the fix only one round was run before stopping the
tests: **the §7 conclusions on the reset must be re-verified from scratch.**

## 10. Capture of the official client (2026-09-30): §9 is wrong

By hooking the `WebSocket` constructor in the official client's renderer
(`slack --remote-debugging-port=9222`) 367 frames of a real huddle session were
captured. **The result refutes §9.**

### What §9 claimed, and which is false

The `SUBSCRIBE_ACK` of the official client is:

```
duplex: 1
allocations: [{ trackLabel: "", streamId: 1, groupId: 1 }]
```

`trackLabel` **empty**, and the `tracks` field **isn't present at all**,
identical to what we receive. The SFU answers everyone this way: it's not a sign
that our track isn't mapped. The eight changes listed in §9 as "tried without
effect" were chasing a nonexistent problem.

### Real differences between the two SUBSCRIBE

The protobuf is almost identical (`duplex`, `mediaType`, `trackLabel`, the ids, the
bitrates, `attendeeId`, `audioHost` match). They differed in:

- `receiveStreamIds: [0]`: they send it, we omitted it. **Fixed.** On its own it
  changes nothing.
- the SDP: they send it in `compressedSdpOffer` (**raw deflate**, 6198 →
  1673 bytes) with an empty `sdpOffer`; we in the clear, having declared
  `wants_compressed_sdp: false`. The server accepts both.

### The difference that mattered: the ICE candidates

```
official:   1 candidate   → typ relay, via Chime's TURN
ours:      18 candidates  → all typ host (192.168.x.x, 10.0.x.x)
```

The TURN credentials we received and passed on, but without setting
`ice_transport_policy` webrtc-rs stays on `all` and offers every local address too,
unreachable for the SFU. **Fixed** with
`RTCIceTransportPolicy::Relay`: now we offer a single relay candidate.

It doesn't solve the signaling collapse, though.

### Cadence: the official client never goes quiet

```
+0.0s --> PING_PONG, JOIN      +0.1s --> SUBSCRIBE
+0.2s --> DATA_MESSAGE         +0.3s --> MEETING_SESSION_TIMING
+0.7s --> DATA_MESSAGE         +1.2s --> CLIENT_METRIC
then CLIENT_METRIC every 1.0 s exactly, PING_PONG every 10 s
```

Between the `SUBSCRIBE` and the first ping we were quiet for **ten seconds**, and the
server closed at three.

Tried sending **empty** `CLIENT_METRIC` every second: the server closes
**earlier**, at ~1.5 s instead of ~3. So it reads the content. The real frames
carry `globalMetrics` and `streamMetricFrames` with PeerConnection statistics.

`CLIENT_METRIC` with real metrics (same shape as the captured one: five
globals, two groups per stream) **changes nothing**. Nor does aligning the
`JOIN` (`serverSideNetworkAdaption: 4`, `supportedServerSideNetworkAdaptions`,
real SDK version) or the `receiveStreamIds`.

### The bisection that really narrows the field (2026-09-30)

```
slack call --connect --hold 20      → signaling up for all 20 s,
                                      server ping at 10 s and 20 s
slack call --audio   --hold 20      → reset at ~2 s
```

**Signaling alone is perfectly fine. It's the media that makes us close.** Everything
about the `JOIN` frame, the `CLIENT_METRIC` cadence and the `SUBSCRIBE` content is
therefore out of scope: the answer is in the SDP or in the RTP flow.

Measured that **we truly transmit**: ~50 packets per second out, zero in, normal
since we're alone in the room.

### The remaining difference in the SDP

```
official   m=audio ... 63 111 9 0 8 13 110 126
           a=rtpmap:63 red/48000/2      ← RED, audio redundancy
           a=fmtp:63 111/111
           a=fmtp:111 maxaveragebitrate=40000;minptime=10;useinbandfec=1
ours       m=audio ... 111
           a=fmtp:111 minptime=10;useinbandfec=1
```

The SFU **accepts** our offer with only Opus (it answers `m=audio ... 111`), so RED
isn't formally mandatory. But careful: offering it without implementing it would make
things worse. RED (RFC 2198) encapsulates the current Opus frame plus the previous
one; `TrackLocalStaticSample` with mime `audio/opus` writes the bare payload. If we
offer it and the SFU picks it, we send packets that aren't RED and the SFU receives
garbage. Packetization must be implemented before offering it.

### The SFU doesn't hear us (2026-09-30)

Decoding `AUDIO_STREAM_ID_INFO` and `AUDIO_METADATA` instead of discarding them:

```
AUDIO_STREAM_ID_INFO:  stream=1 → our attendee
AUDIO_METADATA:        stream=0  volume=0     ← never stream 1
```

The SFU assigns us stream 1 but **never reports a volume for it**, even though we
send ~50 packets per second. It doesn't receive our audio, or doesn't recognize it.

Declaring `audio_muted: false` (option `--unmuted`) changes the server's behavior,
from 4 to 56 `AUDIO_METADATA` in a few seconds, but doesn't solve it. Note that
`muted` was only *declared*: the microphone was opened and transmitted anyway, so we
told the SFU one thing and did another.

The two SDP responses from the SFU are otherwise structurally identical
(`sendrecv`, `setup:passive`, srflx and host candidates): the only difference is RED.

### On-the-wire capture (2026-09-30): the SFU never answers us

`tcpdump` during an official-client huddle and one of our probes, same channel, a
few seconds apart:

```
OFFICIAL    out=956   in=3611      toward/from 99.77.157.55:3478 (TURN)
OURS        out=558   in=18        toward/from 99.77.158.160:3478
```

Our packets **really do leave** (558 in ~10 s), toward the right TURN, and with
sizes similar to theirs (140-150 bytes versus 145-148). What's missing is the
return: **18 packets inbound in total**, that is, just enough for ICE and the DTLS
handshake. After that the SFU sends us nothing more, not even an RTCP, which it
would send to anyone receiving anything.

Verified it's not a parameter mismatch: the `RTCRtpSender` produces SSRC
`3811120763` and payload type `111`, identical to those declared in our SDP and
accepted in the SFU's response.

**Where the problem remains.** ICE and DTLS complete (the PeerConnection reaches
`connected`), the packets leave in the right shape, but the SFU doesn't process
them and doesn't answer. The hypothesis that holds is that it can't **decrypt**
the SRTP flow: it would be invisible from here, it would explain the total silence
in return, and it would explain the teardown a few seconds later.

Things to look at in that direction: the negotiated SRTP profile, the DTLS
fingerprint, and whether the official client negotiates a profile that webrtc-rs
doesn't offer.

### Note on the counters

webrtc-rs's `get_stats()` reports `bytes_sent`/`bytes_received` as zero on the
candidate pairs: that measurement is unusable. The packets delivered to the
sender (`OutboundRTP.packets_sent`) instead keep rising even after the connection
has dropped, so they aren't proof that something went out. To know what really
passes you need `/proc/net/snmp` or `tcpdump`.

### Two other things that emerged

- The official client's `rooms.join` sends `regions: eu-south-1`. We send the field
  empty, and the signaling ends up on a different region each attempt. The comment
  in our code ("it sends it empty when it hasn't resolved the region") is wrong.
- **How a huddle closes**: neither via HTTP nor from the main WebSocket. On both
  you only see *incoming* events (`sh_room_leave`, `sh_room_update`). The room ends
  when the Chime attendee leaves, that is, with the `LEAVE` on the signaling socket
  and its closure. `rooms.leave` via REST exists but the client doesn't use it, and
  rejects every `reason` tried: it's a wrong path.

### Tools

In `../capture/` (outside the repository): `hook.js` captures the frames of a
WebSocket by hooking the constructor before TLS, `ws-main.js` does the same on the
main socket by reloading the page, `decode.js` and `summary.js` decode with this
project's `.proto`, `api.js` logs the REST calls with the body.

---

## 9. Real state of the problem (2026-09-04, corrected bench): SUPERSEDED by §10

With the media truly kept open and the state sampled every second:

```
[ 0s] media=connected    signaling=up
[ 2s] media=connected    signaling=up
      ⚠ signaling down: Connection reset without closing handshake
[ 3s] media=connected    signaling=down
[ 5s] media=connected    signaling=down
[ 6s] media=disconnected signaling=down
```

**Signaling drops first, ~3 s after the media's `connected`; the media follows ~3-5 s
later** (ICE timeout, a consequence and not the cause). The test-bench bug described
in §8 was real but wasn't the cause: the reset stays identical once fixed.

### The real clue

Contents of the `SUBSCRIBE_ACK`:

```
duplex = Some(1)   allocations = 1   tracks = 0
allocation: SdkStreamAllocation { track_label: Some(""), stream_id: Some(1), group_id: Some(1) }
```

We send `track_label: "AmazonChimeExpressAudio"` in the
`SdkStreamDescriptor`, and the SFU answers with an **empty label and zero
`SdkTrackMapping`**. `tracks` is the track→SSRC mapping: without it, the server
doesn't know which stream the RTP arriving to it belongs to. Consistent with a
teardown a few seconds later, when it concludes that it receives nothing on the
allocated stream.

The problem is therefore **upstream** of everything tried so far, and the
following changes don't touch it (all verified with no effect):

- RTP header extensions (`sdes:mid`, `ssrc-audio-level`, `abs-send-time`,
  `transport-wide-cc`): the SFU accepts only two of them and nothing changes;
- `rtcp-fb:111 transport-cc`;
- an `m=video` in BUNDLE;
- removal of the data channel;
- correct Opus encoding (it was a real bug anyway);
- empty `CLIENT_METRIC` frames every second;
- removal of `X-Amzn-Chime-Send-Close-On-Error`.

### Next step

We need to see the **official client's** `SUBSCRIBE` and `SUBSCRIBE_ACK`, not its
SDP: the difference is in the correspondence between `SdkStreamDescriptor` and
SDP, not in the SDP attributes taken alone. You capture it by hooking the
`WebSocket` constructor in the renderer via CDP (`slack
--remote-debugging-port=9222`) and logging the binary frames before encryption,
then decoding them with the types in `proto/`.

Hypotheses to check there: how the official client correlates the `track_label`
with the SDP's `a=msid`, and whether it sends a second `SUBSCRIBE` after the `INDEX`.

## 11. Incoming calls (2026-10-01)

Captured from the official client's WebSocket during a real huddle. There are four
events, none documented:

| Event | When | Fields needed |
| --- | --- | --- |
| `huddle_invite` | they call us **directly** (two-party conversations only) | `channel_id`, `sender_user_id`, `call_id`, and a complete `free_willy` block |
| `huddle_invite_cancel` | they hung up before the answer | `channel_id` |
| `sh_room_update` | the room changes state | `huddle.channel_id`, `room` |
| `sh_room_join` / `sh_room_leave` | someone enters or leaves | `huddle.channel_id`, `room`, `user` |

The three `sh_room_*` carry the same complete `room` object, so they're handled the
same way: the previous snapshot is replaced. Inside `room` what matters is `id`,
`participants` (who's there **now**, not `participant_history`) and `has_ended`.

Two non-obvious things:

- **In a channel you don't get invited.** `huddle_invite` arrives only for
  two-party conversations. In a channel the call is just visible: ringing
  something for every huddle opened in one of the channels you're subscribed to
  would be unbearable. That's why only the invite rings.
- **The first `sh_room_update` of a freshly created huddle has `participants`
  empty** (`is_prewarmed: true`): the room exists but there's still nobody. It's
  the same shape as a room that has emptied out, and it's handled the same,
  there's no call to join.

`huddle_invite` already contains `free_willy.meeting` and `free_willy.attendee`,
so you could enter the call **without** going through `rooms.join`. We don't:
`rooms.join` on the channel also works for channel huddles, for which no invite
arrives, and a single path is a path that breaks in one place only.

Implementation: `src/app/huddles.rs` (event reading) and
`App::apply_huddle` (state).
