//! [`Client`]: the logged-in Zalo account and everything it can do.

use crate::api::{self, Friend, GroupInfo, Member};
use crate::credentials::{CookieJar, Credentials};
use crate::error::{Error, Result};
use crate::http::{Http, DEFAULT_USER_AGENT};
use crate::login_qr::{self, QrEvent};
use crate::mention::Mention;
use crate::message::{id_of, ThreadType, ZaloRef};
use crate::react::ReactionIcon;
use crate::session::{self, Session};
use crate::sticker::{self, Sticker};
use crate::upload::{self, FileWaiters, SentAttachment, ShareLimits, Upload, UploadKind};
use crate::ws::{self, Cmd, CmdSender, ListenOptions, Listener};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The answer to a send: what Zalo says about the message it took.
#[derive(Clone, Debug, PartialEq)]
pub struct SentMessage {
    /// The global id Zalo gave the message (`msgId`), when the answer has one. The same id
    /// arrives with the message's echo on the listener.
    pub msg_id: Option<String>,
    /// The `clientId` the request carried (the time in ms): the message's `cliMsgId`, which a
    /// later quote or reaction of it needs.
    pub client_id: i64,
    /// The undecoded answer.
    pub raw: Value,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

struct Inner {
    http: Http,
    session: Session,
    imei: String,
    user_agent: String,
    language: String,
    /// Where history requests go: the listener started last.
    listener: Mutex<Option<CmdSender>>,
    /// Uploads waiting for the listener to see their "file done" frame.
    file_waiters: Arc<FileWaiters>,
}

/// A logged-in Zalo web session. Cheap to clone (all clones share the session).
///
/// ```no_run
/// # async fn demo(saved: zca_rust::Credentials) -> zca_rust::Result<()> {
/// let client = zca_rust::Client::from_credentials(saved).await?;
/// println!("logged in as uid {}", client.uid());
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("uid", &self.inner.session.uid).finish_non_exhaustive()
    }
}

impl Client {
    /// Log in by QR code: `on_qr` is called with each [`QrEvent`] (show the QR, tell the user
    /// to scan it with the Zalo app and confirm on the phone). Returns the [`Credentials`] to
    /// save; pass them to [`Client::from_credentials`] to use the session.
    ///
    /// Nothing is written anywhere: persisting the credentials is up to the caller.
    pub async fn login_qr(on_qr: impl FnMut(QrEvent)) -> Result<Credentials> {
        let http = Http::new(DEFAULT_USER_AGENT, CookieJar::new())?;
        login_qr::login_qr(&http, on_qr).await?;
        Ok(Credentials::new(http.cookies()?))
    }

    /// Cookie login: trade saved [`Credentials`] for a live session. Fails with [`Error::Auth`]
    /// when the cookies are expired (log in by QR again).
    pub async fn from_credentials(creds: Credentials) -> Result<Client> {
        let http = Http::new(&creds.user_agent, creds.cookies.clone())?;
        let session = session::login_cookie(&http, &creds).await?;
        Ok(Client { inner: Arc::new(Inner { http, session, imei: creds.imei, user_agent: creds.user_agent, language: creds.language, listener: Mutex::new(None), file_waiters: Arc::default() }) })
    }

    /// The credentials as they are now, with the cookies the server refreshed during this
    /// session. Save these after logging in to keep the session alive.
    pub fn credentials(&self) -> Result<Credentials> {
        let i = &self.inner;
        Ok(Credentials { imei: i.imei.clone(), user_agent: i.user_agent.clone(), language: i.language.clone(), cookies: i.http.cookies()? })
    }

    /// The logged-in account's uid.
    pub fn uid(&self) -> &str {
        &self.inner.session.uid
    }

    /// The device id of this session.
    pub fn imei(&self) -> &str {
        &self.inner.imei
    }

    /// The user agent of this session.
    pub fn user_agent(&self) -> &str {
        &self.inner.user_agent
    }

    /// The websocket endpoints Zalo gave this session.
    pub fn ws_endpoints(&self) -> &[String] {
        &self.inner.session.ws_urls
    }

    // ------------------------------------------------------------ realtime

    /// Open the realtime connection with the default [`ListenOptions`]. Needs a running tokio
    /// runtime. Zalo allows one web listener per account: starting another one elsewhere
    /// (or in the browser) ends this one with [`Event::Stopped`](crate::Event::Stopped).
    pub fn listen(&self) -> Result<Listener> {
        self.listen_with(ListenOptions::default())
    }

    /// [`Client::listen`] with options (the reconnect policy). A listener started earlier on
    /// this client is stopped.
    pub fn listen_with(&self, opts: ListenOptions) -> Result<Listener> {
        let i = &self.inner;
        let (listener, cmds) = ws::spawn(&i.http, &i.session, &i.user_agent, opts, i.file_waiters.clone())?;
        *i.listener.lock().map_err(|_| Error::protocol("listener lock poisoned"))? = Some(cmds);
        Ok(listener)
    }

    /// Ask for a page of history: the newest messages of all DMs or all groups, or (with
    /// `last_id`, the oldest message id of the previous page) the page before it. The answer
    /// arrives on the listener as [`Event::History`](crate::Event::History). Err
    /// [`Error::NotListening`] when no listener runs.
    pub async fn history(&self, thread_type: ThreadType, last_id: Option<&str>) -> Result<()> {
        let tx = self.inner.listener.lock().map_err(|_| Error::protocol("listener lock poisoned"))?.clone();
        tx.ok_or(Error::NotListening)?
            .send(Cmd::RequestOld { thread_type, last_id: last_id.map(String::from) })
            .await
            .map_err(|_| Error::NotListening)
    }

    // ------------------------------------------------------------ sending

    async fn run(&self, p: &api::Planned) -> Result<Value> {
        api::run(&self.inner.http, &self.inner.session, p).await
    }

    fn sent(data: Value, client_id: i64) -> SentMessage {
        SentMessage { msg_id: id_of(&data["msgId"]), client_id, raw: data }
    }

    /// Send a text message. `thread_id` is the other person's uid, or the group id for
    /// [`ThreadType::Group`].
    pub async fn send_text(&self, thread_id: &str, thread_type: ThreadType, text: &str) -> Result<SentMessage> {
        self.send_message(thread_id, thread_type, text, None, &[]).await
    }

    /// Answer `quote` (a [`ZaloRef`], see [`ZaloMessage::reference`](crate::ZaloMessage::reference))
    /// with a text, as a quote card.
    pub async fn send_quote(&self, thread_id: &str, thread_type: ThreadType, text: &str, quote: &ZaloRef) -> Result<SentMessage> {
        self.send_message(thread_id, thread_type, text, Some(quote), &[]).await
    }

    /// Send a group text that tags people (DMs carry no mentions: they are ignored there).
    pub async fn send_mention(&self, thread_id: &str, thread_type: ThreadType, text: &str, mentions: &[Mention]) -> Result<SentMessage> {
        self.send_message(thread_id, thread_type, text, None, mentions).await
    }

    /// Send a text, optionally as a quote and/or with mentions (zca-js `sendMessage`).
    pub async fn send_message(&self, thread_id: &str, thread_type: ThreadType, text: &str, quote: Option<&ZaloRef>, mentions: &[Mention]) -> Result<SentMessage> {
        let client_id = now_ms();
        let planned = api::plan_text(&self.inner.imei, thread_id, thread_type, text, client_id, quote, mentions);
        Ok(Self::sent(self.run(&planned).await?, client_id))
    }

    /// Zalo's own forward of a text to another chat (zca-js `forwardMessage`; it carries text only).
    pub async fn forward(&self, to_thread_id: &str, thread_type: ThreadType, text: &str) -> Result<SentMessage> {
        let client_id = now_ms();
        let planned = api::plan_forward(&self.inner.imei, to_thread_id, thread_type, text, client_id);
        let data = self.run(&planned).await?;
        let msg_id = api::forward_result(&data)?;
        Ok(SentMessage { msg_id, client_id, raw: data })
    }

    /// Recall (zca-js `undo`) one of our own messages for everyone; `msg_id` and `cli_msg_id`
    /// are its global and client ids.
    pub async fn undo(&self, thread_id: &str, thread_type: ThreadType, msg_id: &str, cli_msg_id: &str) -> Result<()> {
        let planned = api::plan_undo(&self.inner.imei, thread_id, thread_type, msg_id, cli_msg_id, now_ms());
        self.run(&planned).await.map(|_| ())
    }

    /// React to a message (zca-js `addReaction`); [`ReactionIcon::None`] takes our reaction
    /// back. Only the icons of [`ReactionIcon::panel`] can be sent.
    pub async fn add_reaction(&self, thread_id: &str, thread_type: ThreadType, msg_id: &str, cli_msg_id: &str, icon: &ReactionIcon) -> Result<()> {
        if matches!(icon, ReactionIcon::Other(_)) {
            return Err(Error::invalid("only the icons of the reaction panel can be sent"));
        }
        let (Ok(g), Ok(c)) = (msg_id.parse::<u64>(), cli_msg_id.parse::<u64>()) else {
            return Err(Error::invalid("bad message id"));
        };
        let planned = api::plan_reaction(&self.inner.imei, thread_id, thread_type, g, c, icon.code(), icon.r_type(), now_ms());
        self.run(&planned).await.map(|_| ())
    }

    /// Send a sticker (zca-js `sendSticker`): needs `id`, `cate_id` and `kind` of the [`Sticker`].
    pub async fn send_sticker(&self, thread_id: &str, thread_type: ThreadType, sticker: &Sticker) -> Result<SentMessage> {
        let client_id = now_ms();
        let planned = sticker::plan_send(&self.inner.imei, thread_id, thread_type, sticker, client_id)?;
        Ok(Self::sent(self.run(&planned).await?, client_id))
    }

    // ------------------------------------------------------------ images, files, GIFs

    /// What this account may share: the size limit, the blacklisted extensions, the chunk size
    /// (the `sharefile` block of the login settings). Err when the settings carry none.
    pub fn share_limits(&self) -> Result<ShareLimits> {
        ShareLimits::from_settings(&self.inner.session.settings)
    }

    fn upload_ctx<'a>(&'a self, limits: &'a ShareLimits) -> upload::Ctx<'a> {
        let i = &self.inner;
        upload::Ctx { http: &i.http, session: &i.session, imei: &i.imei, limits, waiters: &i.file_waiters }
    }

    /// Whether a listener is running to hear Zalo's "file done" frames.
    fn listening(&self) -> bool {
        self.inner.listener.lock().ok().and_then(|g| g.as_ref().map(|tx| !tx.is_closed())).unwrap_or(false)
    }

    /// Send a photo (`jpg`, `jpeg`, `png` or `webp`) with `caption` as its description (may be
    /// empty). Refused before any upload when it is over [`Client::share_limits`].
    pub async fn send_image(&self, thread_id: &str, thread_type: ThreadType, image: Upload, caption: &str) -> Result<SentAttachment> {
        let limits = self.share_limits()?;
        self.upload_ctx(&limits).send_image(thread_id, thread_type, image, caption).await
    }

    /// Send a file of any kind. Zalo says where it stored the file on the websocket, so the
    /// listener must be running ([`Client::listen`]) or this is [`Error::NotListening`]. A
    /// `caption` is sent as a text message of its own, just before the file (as zca-js does).
    pub async fn send_file(&self, thread_id: &str, thread_type: ThreadType, file: Upload, caption: &str) -> Result<SentAttachment> {
        let limits = self.share_limits()?;
        if !self.listening() {
            return Err(Error::NotListening);
        }
        self.upload_ctx(&limits).send_file(thread_id, thread_type, file, caption).await
    }

    /// Send a `.gif`. A `caption` is sent as a text message of its own, just before the GIF.
    pub async fn send_gif(&self, thread_id: &str, thread_type: ThreadType, gif: Upload, caption: &str) -> Result<SentAttachment> {
        let limits = self.share_limits()?;
        self.upload_ctx(&limits).send_gif(thread_id, thread_type, gif, caption).await
    }

    /// Send `source` the way zca-js picks by extension: `.gif` as a GIF, `jpg` / `jpeg` / `png` /
    /// `webp` as a photo, anything else as a file (which needs the listener, see [`Client::send_file`]).
    pub async fn send_attachment(&self, thread_id: &str, thread_type: ThreadType, source: Upload, caption: &str) -> Result<SentAttachment> {
        match UploadKind::of_extension(&source.name_and_extension()?.1) {
            UploadKind::Image => self.send_image(thread_id, thread_type, source, caption).await,
            UploadKind::Gif => self.send_gif(thread_id, thread_type, source, caption).await,
            _ => self.send_file(thread_id, thread_type, source, caption).await,
        }
    }

    // ------------------------------------------------------------ stickers

    /// The stickers `keyword` suggests, with their details (at most [`sticker::SEARCH_MAX`]).
    pub async fn search_stickers(&self, keyword: &str) -> Result<Vec<Sticker>> {
        sticker::search(&self.inner.http, &self.inner.session, &self.inner.imei, keyword).await
    }

    /// The details of one sticker; `None` when Zalo has no sendable sticker with that id.
    pub async fn sticker_detail(&self, id: u64) -> Result<Option<Sticker>> {
        sticker::detail(&self.inner.http, &self.inner.session, id).await
    }

    // ------------------------------------------------------------ people and groups

    /// Every friend of the account.
    pub async fn friends(&self) -> Result<Vec<Friend>> {
        api::friends(&self.inner.http, &self.inner.session, &self.inner.imei).await
    }

    /// The id of every group the account is in.
    pub async fn group_ids(&self) -> Result<Vec<String>> {
        api::all_group_ids(&self.inner.http, &self.inner.session).await
    }

    /// group id -> name, for the given groups.
    pub async fn group_names(&self, ids: &[String]) -> Result<HashMap<String, String>> {
        api::group_names(&self.inner.http, &self.inner.session, ids).await
    }

    /// One group's details and member ids.
    pub async fn group_info(&self, group_id: &str) -> Result<GroupInfo> {
        api::group_info(&self.inner.http, &self.inner.session, group_id).await
    }

    /// The members of a group that have a name: from the group info, then from `known`
    /// (uid -> name, say the friends), then looked up. Nameless members are left out.
    pub async fn group_members(&self, group_id: &str, known: &HashMap<String, String>) -> Result<Vec<Member>> {
        api::group_members(&self.inner.http, &self.inner.session, group_id, known).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client with a made-up session: enough to check what is refused before any request is made.
    fn offline_client() -> Client {
        let session = Session { uid: "1".into(), secret_key: String::new(), ws_urls: vec![], service_map: Value::Null, settings: Value::Null };
        let http = Http::new(DEFAULT_USER_AGENT, CookieJar::new()).unwrap();
        Client { inner: Arc::new(Inner { http, session, imei: "IMEI".into(), user_agent: DEFAULT_USER_AGENT.into(), language: "vi".into(), listener: Mutex::new(None), file_waiters: Arc::default() }) }
    }

    #[tokio::test]
    async fn bad_requests_are_refused_before_the_network() {
        let c = offline_client();
        assert_eq!((c.uid(), c.imei(), c.ws_endpoints().len()), ("1", "IMEI", 0));
        let invalid = |e: Error| matches!(e, Error::Invalid(_));
        // ids that are not numbers, and icons outside the panel, cannot be sent as reactions
        let heart = ReactionIcon::Heart;
        assert!(invalid(c.add_reaction("42", ThreadType::User, "x", "1", &heart).await.unwrap_err()));
        assert!(invalid(c.add_reaction("42", ThreadType::User, "1", "1", &ReactionIcon::Other("/-ok".into())).await.unwrap_err()));
        // a sticker needs its id and type
        assert!(invalid(c.send_sticker("42", ThreadType::User, &Sticker::default()).await.unwrap_err()));
        assert!(invalid(c.search_stickers("  ").await.unwrap_err()));
    }

    /// A client whose login settings carry `sharefile` limits (1 MB, 4-byte chunks, no `exe`).
    fn sharing_client() -> Client {
        let settings = serde_json::json!({"features": {"sharefile": {"max_file": 20, "max_size_share_file_v3": 1, "chunk_size_file": 4, "restricted_ext_file": ["exe"]}}});
        let session = Session { uid: "1".into(), secret_key: String::new(), ws_urls: vec![], service_map: Value::Null, settings };
        let http = Http::new(DEFAULT_USER_AGENT, CookieJar::new()).unwrap();
        Client { inner: Arc::new(Inner { http, session, imei: "IMEI".into(), user_agent: DEFAULT_USER_AGENT.into(), language: "vi".into(), listener: Mutex::new(None), file_waiters: Arc::default() }) }
    }

    #[tokio::test]
    async fn uploads_are_refused_before_the_network() {
        let png = |n: &str| Upload::bytes(n, vec![1u8, 2, 3]);
        // no sharefile settings: nothing can be checked, so nothing is sent
        let c = offline_client();
        assert!(matches!(c.share_limits(), Err(Error::Protocol(_))));
        assert!(matches!(c.send_image("42", ThreadType::User, png("a.png"), "").await, Err(Error::Protocol(_))));

        let c = sharing_client();
        assert_eq!(c.share_limits().unwrap().max_bytes(), 1024 * 1024);
        let invalid = |e: Error| matches!(e, Error::Invalid(_));
        let over = || Upload::bytes("big.bin", vec![0u8; 1024 * 1024 + 1]);
        assert!(invalid(c.send_image("42", ThreadType::User, Upload::bytes("big.png", vec![0u8; 1024 * 1024 + 1]), "").await.unwrap_err()));
        assert!(invalid(c.send_image("42", ThreadType::User, Upload::bytes("a.pdf", vec![1]), "").await.unwrap_err()), "not an image");
        assert!(invalid(c.send_image("", ThreadType::User, png("a.png"), "").await.unwrap_err()), "no thread");
        assert!(invalid(c.send_gif("42", ThreadType::Group, png("a.png"), "").await.unwrap_err()), "not a gif");
        assert!(invalid(c.send_gif("42", ThreadType::Group, Upload::bytes("big.gif", vec![0u8; 1024 * 1024 + 1]), "").await.unwrap_err()));
        // a file needs the listener first; the size and blacklist checks follow before anything is uploaded
        assert!(matches!(c.send_file("42", ThreadType::User, over(), "").await, Err(Error::NotListening)));
        assert!(matches!(c.send_attachment("42", ThreadType::User, over(), "").await, Err(Error::NotListening)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        *c.inner.listener.lock().unwrap() = Some(tx);
        assert!(c.listening());
        let e = c.send_file("42", ThreadType::User, over(), "").await.unwrap_err();
        assert_eq!(e.to_string(), "File big.bin size exceed maximum size of 1MB");
        let e = c.send_attachment("42", ThreadType::User, Upload::bytes("run.EXE", vec![1]), "").await.unwrap_err();
        assert_eq!(e.to_string(), "File extension \"exe\" is not allowed");
        assert!(invalid(c.send_file("42", ThreadType::User, Upload::path("/no/such/file.bin"), "").await.unwrap_err()));
        // a listener that has ended does not count
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        *c.inner.listener.lock().unwrap() = Some(tx);
        drop(rx);
        assert!(!c.listening());
    }

    #[tokio::test]
    async fn history_needs_a_listener_and_a_listener_needs_endpoints() {
        let c = offline_client();
        assert!(matches!(c.history(ThreadType::User, None).await, Err(Error::NotListening)));
        assert!(matches!(c.listen(), Err(Error::Protocol(_))), "no websocket endpoints");
    }

    #[test]
    fn credentials_keep_the_session_identity_and_never_print_secrets() {
        let c = offline_client();
        let creds = c.credentials().unwrap();
        assert_eq!((creds.imei.as_str(), creds.user_agent.as_str(), creds.language.as_str()), ("IMEI", DEFAULT_USER_AGENT, "vi"));
        assert_eq!(format!("{c:?}"), "Client { uid: \"1\", .. }");
    }
}
