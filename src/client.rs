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

fn now_ms() -> i64 {
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
        Ok(Client { inner: Arc::new(Inner { http, session, imei: creds.imei, user_agent: creds.user_agent, language: creds.language, listener: Mutex::new(None) }) })
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
        let (listener, cmds) = ws::spawn(&i.http, &i.session, &i.user_agent, opts)?;
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
        Client { inner: Arc::new(Inner { http, session, imei: "IMEI".into(), user_agent: DEFAULT_USER_AGENT.into(), language: "vi".into(), listener: Mutex::new(None) }) }
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
