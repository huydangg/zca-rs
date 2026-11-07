//! `zca-rust`: an unofficial Zalo Web API client.
//!
//! A Rust take on [zca-js](https://github.com/RFS-ADRENO/zca-js): it talks to Zalo the way
//! the official web client (`chat.zalo.me`) does, so it can log in by QR code or saved
//! cookies, listen for messages in real time and send messages, quotes, mentions,
//! reactions, stickers, images, files, GIFs and recalls.
//!
//! **Unofficial.** Zalo has no public API for this; it can change or block it at any time,
//! and automated use (spam especially) can get an account locked. You are responsible for
//! complying with Zalo's terms.
//!
//! The crate never reads or writes files unless you call [`Credentials::save_private`] /
//! [`Credentials::load`], or send a file by path ([`Upload::path`]): [`Credentials`] are plain serializable data that you can also store
//! wherever you want. They hold your live session cookies, so keep the file private.
//!
//! The listen loop below is **read-only**: it prints what arrives and sends nothing. (Your own
//! sent messages come back as events too, so never send from inside such a loop without
//! checking `m.is_self`.)
//!
//! ```no_run
//! use zca_rust::{Client, Credentials, Event};
//!
//! # async fn demo() -> zca_rust::Result<()> {
//! // First run: scan the QR with the Zalo app, then save the credentials (file mode 0600).
//! let creds = Client::login_qr(|ev| println!("{ev:?}")).await?;
//! creds.save_private("zalo.json")?;
//!
//! // Later runs: log in with the saved cookies and listen (read-only).
//! let creds = Credentials::load("zalo.json")?;
//! let client = Client::from_credentials(creds).await?;
//! let mut events = client.listen()?;
//! while let Some(event) = events.recv().await {
//!     if let Event::Message(m) | Event::GroupMessage(m) = event {
//!         println!("{}: {:?}", m.d_name, m.text());
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! Sending is a separate, deliberate call. This sends one message to the thread id *you*
//! put in; do not call it from the listen loop above.
//!
//! ```no_run
//! use zca_rust::{Client, ThreadType};
//!
//! # async fn demo(client: Client) -> zca_rust::Result<()> {
//! // Replace with the id of a chat you own and want to message.
//! client.send_text("<thread id>", ThreadType::User, "hello").await?;
//! # Ok(()) }
//! ```
//!
//! Images, GIFs and files are sent the same way ([`Client::send_image`], [`Client::send_gif`],
//! [`Client::send_file`], or [`Client::send_attachment`] to let the extension pick). They are
//! checked against [`Client::share_limits`] before anything is uploaded; a file also needs the
//! listener running, because Zalo reports where it stored the file on the websocket.
//!
//! ```no_run
//! use zca_rust::{Client, ThreadType, Upload};
//!
//! # async fn demo(client: Client) -> zca_rust::Result<()> {
//! // Replace with the id of a chat you own and want to send to.
//! let sent = client.send_attachment("<thread id>", ThreadType::User, Upload::path("photo.jpg"), "caption").await?;
//! println!("sent {} ({} bytes)", sent.file_name, sent.total_size);
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod api;
mod client;
mod credentials;
mod crypto;
mod error;
mod events;
mod http;
mod login_qr;
mod mention;
mod message;
mod params;
#[cfg(feature = "qr")]
pub mod qr;
mod react;
mod session;
mod sticker;
mod upload;
mod ws;

pub use api::{Friend, GroupInfo, Member};
pub use client::{Client, SentMessage};
pub use credentials::{generate_imei, CookieJar, Credentials};
pub use error::{Error, Result};
pub use http::DEFAULT_USER_AGENT;
pub use login_qr::QrEvent;
pub use mention::{Mention, MentionTarget, MENTIONS_MAX};
pub use message::{
    sticker_url, Attachment, AttachmentKind, Card, Content, Delivered, GifContent, MessageMention, Quote, Seen, StickerContent, ThreadType, Undo,
    ZaloMessage, ZaloRef,
};
pub use react::{Reaction, ReactionIcon};
pub use session::{API_TYPE, API_VERSION};
pub use sticker::{sprite_url, Sticker, SEARCH_MAX, STICKER_URL};
pub use upload::{SentAttachment, ShareLimits, Upload, UploadKind};
pub use ws::{Event, ListenOptions, Listener, ReconnectPolicy};
