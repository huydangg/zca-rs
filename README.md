# zca-rs

Rust crate **`zca-rust`** (import as `zca_rust`).


An **unofficial Zalo Web API client in Rust**: log in by QR code or saved cookies, listen
for messages in real time, send messages, quotes, mentions, reactions, stickers, images, files and GIFs.

It is a **Rust rewrite of [zca-js](https://github.com/RFS-ADRENO/zca-js)** by
RFS-ADRENO, truong9c2208 and JustKemForFun (and contributors), and talks to Zalo the way the
official web client (`chat.zalo.me`) does: the request shapes, the AES parameter crypto, the
websocket framing and the upload flows are ported from zca-js module by module, and several
test vectors were produced by running zca-js's own code to cross-check the port. zca-js is MIT licensed; its notice is reproduced in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Credit and thanks to them; this crate would
not exist without that reverse-engineering work.

## Disclaimer

* **Unofficial.** Zalo has no public API for this. It is reverse-engineered, may break
  at any time, and comes with no guarantees. Not affiliated with, endorsed by or connected
  to Zalo / VNG.
* **You are responsible** for complying with Zalo's terms of service and the law where you live.
* **Automated or spammy use can get your account locked.** Use it for your own account, at a
  human pace. Do not use it to message people who did not ask for it.
* Credentials hold your live session cookies. Treat them like a password and never commit them
  (`Credentials::save_private` writes them with mode `0600`).

## Quick start

```toml
[dependencies]
# not on crates.io yet: use the git repository
zca-rust = { git = "https://github.com/huydangg/zca-rs" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

**1. Log in by QR and save the credentials** (`save_private` writes the file with mode `0600`, also
when it overwrites a more open one; you choose where):

```rust
use zca_rust::{Client, QrEvent};

let creds = Client::login_qr(|event| match event {
    QrEvent::Generated { png, data_url, expires_in } => { /* show the QR: draw `png`, or put `data_url` in an <img> */ }
    QrEvent::Scanned { display_name } => println!("{display_name} scanned it: confirm on the phone"),
    QrEvent::Expired => println!("expired, a new QR follows"),
    _ => {}
})
.await?;
creds.save_private("zalo.json")?;
```

**2. Log in with the saved cookies and listen:**

```rust
use zca_rust::{Client, Credentials, Event};

let creds = Credentials::load("zalo.json")?;
let client = Client::from_credentials(creds).await?;
// keep the cookies Zalo refreshed:
client.credentials()?.save_private("zalo.json")?;

// read-only: this prints what arrives and sends nothing
let mut events = client.listen()?;           // also a `futures::Stream<Item = Event>`
while let Some(event) = events.recv().await {
    match event {
        Event::Message(m) | Event::GroupMessage(m) => println!("{}: {:?}", m.d_name, m.content),
        Event::Stopped { reason } => { eprintln!("stopped: {reason}"); break; }
        _ => {}
    }
}
```

**3. Send a message** (a separate, deliberate call: never send from inside the listen loop,
your own messages come back as events and would loop):

```rust
use zca_rust::ThreadType;

// only to chats you own and want to message; replace the placeholders

client.send_text("<uid>", ThreadType::User, "hello").await?;
client.send_text("<group id>", ThreadType::Group, "hello group").await?;
```

**4. Send an image, a file or a GIF** (same care as above; replace the placeholders; a file needs
the listener running, because Zalo reports where it stored the file on the websocket):

```rust
use zca_rust::Upload;

let limits = client.share_limits()?; // size limit, blacklisted extensions, chunk size
client.send_image("<uid>", ThreadType::User, Upload::path("photo.jpg"), "a caption").await?;
client.send_gif("<group id>", ThreadType::Group, Upload::path("cat.gif"), "").await?;
let _listener = client.listen()?;
client.send_file("<uid>", ThreadType::User, Upload::path("report.pdf"), "").await?;
// or let the extension pick (.gif -> GIF, jpg/png/webp -> photo, else file):
client.send_attachment("<uid>", ThreadType::User, Upload::bytes("note.txt", b"hello".to_vec()), "").await?;
```

A file over `limits.max_bytes()` (or with a blacklisted extension) is refused with `Error::Invalid`
before anything is uploaded. A caption goes with a photo as its description; with a file or a GIF it is
a text message of its own sent just before it, as zca-js does. These calls follow zca-js's upload code
but have not been run against the live service yet.

Runnable versions are in [`examples/`](examples): `login_qr` (prints the QR in the terminal,
saves the credentials to the file you name) and `echo` (prints incoming messages; it is
read-only and never replies).

```text
cargo run -p zca-rust --example login_qr -- zalo.json
cargo run -p zca-rust --example echo -- zalo.json
```

## Features

| Area | Supported | Notes |
| --- | --- | --- |
| QR login | yes | `Client::login_qr`; QR as PNG and `data:` URL, scanned state, expiry |
| Cookie login | yes | `Client::from_credentials`; `Credentials` + `CookieJar` are serde and exportable |
| Realtime listener | yes | `Client::listen` (a `Stream` of `Event`), configurable `ReconnectPolicy`, endpoint rotation |
| Incoming messages | yes | `ZaloMessage` for DMs and groups: text, attachments, stickers, GIFs, links, call logs, quote, mentions; raw JSON kept |
| History | yes | `Client::history` requests a page, it arrives as `Event::History` |
| Recall (undo) events | yes | `Event::Undo` |
| Seen / delivered receipts | yes | `Event::Seen`; `Event::Delivered` is best effort |
| Reaction events | yes | `Event::Reaction` |
| Typing events | no | frames are skipped |
| Send text / quote / mention | yes | `send_text`, `send_quote`, `send_mention`, `send_message` |
| Forward | yes | text only (as zca-js) |
| Recall (undo) | yes | `Client::undo` |
| Reactions | yes | `Client::add_reaction` with the six panel icons (`ReactionIcon`); `None` removes |
| Stickers | yes | `search_stickers`, `sticker_detail`, `send_sticker` |
| Friends / groups | yes | `friends`, `group_ids`, `group_names`, `group_info`, `group_members` |
| GIFs | yes (send: unverified live) | receive: `Content::Gif` (`chat.gif`): link, preview, size. Send: `send_gif` (zca-js's `upthumb` + `gif` upload); no GIF search exists in any source we could check |
| Send images / files | yes (unverified live) | `send_image`, `send_file`, `send_gif`, `send_attachment` (picks by extension); chunked multipart uploads as in zca-js, size and extension limits from the login settings (`share_limits`); a file's upload waits for Zalo's "file done" frame on the listener, so `listen()` must be running |
| Voice, video, location, polls, ... | no | |
| Proxy, custom user agent per call | no | the user agent is part of `Credentials` |

The crate is async on `tokio` with `reqwest` (rustls) and `tokio-tungstenite`. The default
`qr` feature adds `zca_rust::qr` (sample the login QR PNG into a module grid and draw it with
ANSI half blocks); disable it with `default-features = false` to drop `qrcode`, `rqrr`. (`image` stays: it reads
the size of an image or GIF you send from its header.)

## Behaving like the web client

Requests carry the web client's headers, `zpw_ver=685`, `zpw_type=30`, the `zcid` /
`signkey` parameter signing, and an `imei` of the form `<uuid4>-<md5(user agent)>`. A session is
tied to its `imei` and user agent: keep both from the `Credentials` you saved.

## License

MIT, see [LICENSE](LICENSE). Parts are derived from zca-js (MIT); see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
