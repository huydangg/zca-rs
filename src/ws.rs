//! Zalo realtime listener (websocket). Port of zca-js `Listener`.
//!
//! Runs as a task: emits [`Event`]s on a channel and accepts history requests. Reconnects
//! with backoff (see [`ReconnectPolicy`]) and rotates endpoints; stops for good when Zalo
//! says another web session took over (3000) or kicked us (3003) - reconnecting then would
//! just fight the other session.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::error::{Error, Result};
use crate::events::{cmd, decode_event_data, encode_frame, parse_frame};
use crate::http::Http;
use crate::message::{Delivered, Seen, ThreadType, Undo, ZaloMessage};
use crate::react::Reaction;
use crate::session::{Session, API_TYPE, API_VERSION};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMsg;

/// Something that happened on the realtime connection.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Event {
    /// The websocket is up (also after every reconnect).
    Connected,
    /// A direct message (ours too, echoed back from this or another device).
    Message(ZaloMessage),
    /// A group message.
    GroupMessage(ZaloMessage),
    /// A page of history answering [`Client::history`](crate::Client::history), newest first.
    History {
        /// Which kind of threads the page is from.
        thread_type: ThreadType,
        /// The messages of the page.
        messages: Vec<ZaloMessage>,
    },
    /// A reaction was put on (or taken off) a message.
    Reaction(Reaction),
    /// A message was recalled (by its sender, or by us on another device).
    Undo(Undo),
    /// Someone saw our messages (read receipt).
    Seen(Seen),
    /// Our messages reached someone's device.
    Delivered(Delivered),
    /// The connection dropped.
    Disconnected {
        /// Why (the error or the close reason).
        reason: String,
    },
    /// The listener waits `delay` and then reconnects for the `attempt`th time.
    Reconnecting {
        /// 1 for the first retry after a working connection.
        attempt: u32,
        /// How long it waits first.
        delay: Duration,
    },
    /// The listener gave up for good (another web session took over, we were kicked, the
    /// reconnect policy ran out, or the account's `Listener` was replaced). No more events follow.
    Stopped {
        /// Why.
        reason: String,
    },
}

/// How the listener behaves when the connection drops: wait `initial_delay`, doubling up to
/// `max_delay`, for at most `max_attempts` retries in a row. A connection that stayed up for a
/// while starts the count again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Retries in a row before giving up with [`Event::Stopped`]; `None` retries forever.
    pub max_attempts: Option<u32>,
    /// Wait before the first retry.
    pub initial_delay: Duration,
    /// The wait never grows beyond this.
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    /// Forever, 2 s first, then 4 s, 8 s ... up to 60 s.
    fn default() -> Self {
        ReconnectPolicy { max_attempts: None, initial_delay: Duration::from_secs(2), max_delay: Duration::from_secs(60) }
    }
}

impl ReconnectPolicy {
    /// Never reconnect: the first drop ends the listener.
    pub fn never() -> Self {
        ReconnectPolicy { max_attempts: Some(0), ..Self::default() }
    }

    /// The wait before retry number `attempt` (1-based).
    fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.saturating_sub(1).min(16)).unwrap_or(u32::MAX);
        self.initial_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Options of [`Client::listen_with`](crate::Client::listen_with).
#[derive(Clone, Debug, Default)]
pub struct ListenOptions {
    /// What to do when the connection drops.
    pub reconnect: ReconnectPolicy,
}

/// The stream of [`Event`]s of one realtime connection. Dropping it stops the connection.
///
/// It implements [`Stream`]; [`Listener::recv`] is the same without importing `StreamExt`.
pub struct Listener {
    rx: mpsc::Receiver<Event>,
}

impl Listener {
    /// The next event; `None` once the listener has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

impl Stream for Listener {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.rx.poll_recv(cx)
    }
}

/// A request to the running listener.
#[derive(Debug)]
pub(crate) enum Cmd {
    RequestOld { thread_type: ThreadType, last_id: Option<String> },
}

pub(crate) type CmdSender = mpsc::Sender<Cmd>;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn ws_url(base: &str) -> Result<String> {
    let mut u = reqwest::Url::parse(base).map_err(Error::bad_url)?;
    u.query_pairs_mut()
        .append_pair("zpw_ver", &API_VERSION.to_string())
        .append_pair("zpw_type", &API_TYPE.to_string())
        .append_pair("t", &now_ms().to_string());
    Ok(u.to_string())
}

/// A connection that lasted this long counts as working: the retry count starts again.
const STABLE: Duration = Duration::from_secs(30);

/// Spawn the listener task (needs a running tokio runtime). Dropping the returned
/// [`Listener`] (or the sender) stops it.
pub(crate) fn spawn(http: &Http, session: &Session, user_agent: &str, opts: ListenOptions) -> Result<(Listener, CmdSender)> {
    if session.ws_urls.is_empty() {
        return Err(Error::protocol("login gave no websocket endpoints"));
    }
    let cookie = http.cookie_header("https://chat.zalo.me")?;
    let (ev_tx, ev_rx) = mpsc::channel(512);
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let urls = session.ws_urls.clone();
    let ping_ms = session.settings["features"]["socket"]["ping_interval"]
        .as_u64()
        .unwrap_or(20_000)
        .max(5_000);
    let uid = session.uid.clone();
    let ua = user_agent.to_string();
    tokio::spawn(async move {
        let mut cmd_rx = cmd_rx;
        let mut attempt = 0u32;
        let mut endpoint = 0usize;
        loop {
            let started = tokio::time::Instant::now();
            let res = run_once(&urls[endpoint % urls.len()], &cookie, &ua, &uid, ping_ms, &ev_tx, &mut cmd_rx).await;
            let reason = match res {
                Ok(Stop::Final(why)) => {
                    let _ = ev_tx.send(Event::Stopped { reason: why }).await;
                    return;
                }
                Ok(Stop::Closed(why)) | Err(why) => {
                    if ev_tx.is_closed() {
                        return;
                    }
                    format!("{why}")
                }
            };
            let _ = ev_tx.send(Event::Disconnected { reason: reason.clone() }).await;
            if started.elapsed() >= STABLE {
                attempt = 0;
            }
            attempt += 1;
            endpoint += 1; // rotate
            if opts.reconnect.max_attempts.is_some_and(|max| attempt > max) {
                let _ = ev_tx.send(Event::Stopped { reason: format!("not reconnecting: {reason}") }).await;
                return;
            }
            let delay = opts.reconnect.delay(attempt);
            let _ = ev_tx.send(Event::Reconnecting { attempt, delay }).await;
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = ev_tx.closed() => return,
            }
        }
    });
    Ok((Listener { rx: ev_rx }, cmd_tx))
}

enum Stop {
    Closed(Error),
    Final(String),
}

/// The items of a `msgs` / `groupMsgs` list as events: recalls take a message back, the rest are messages.
async fn send_messages(ev: &mpsc::Sender<Event>, list: &Value, thread_type: ThreadType, uid: &str, history: bool) {
    let mut page: Vec<ZaloMessage> = Vec::new();
    for raw in list.as_array().into_iter().flatten() {
        if let Some(u) = Undo::from_json(raw, thread_type, uid) {
            let _ = ev.send(Event::Undo(u)).await;
        } else if let Some(m) = ZaloMessage::from_json(raw, thread_type, uid) {
            if history {
                page.push(m);
            } else {
                let _ = ev.send(if thread_type.is_group() { Event::GroupMessage(m) } else { Event::Message(m) }).await;
            }
        }
    }
    if history {
        let _ = ev.send(Event::History { thread_type, messages: page }).await;
    }
}

async fn run_once(
    base: &str,
    cookie: &str,
    ua: &str,
    uid: &str,
    ping_ms: u64,
    ev: &mpsc::Sender<Event>,
    cmds: &mut mpsc::Receiver<Cmd>,
) -> Result<Stop> {
    let mut req = ws_url(base)?.into_client_request()?;
    let h = req.headers_mut();
    h.insert("origin", HeaderValue::from_static("https://chat.zalo.me"));
    h.insert("user-agent", HeaderValue::from_str(ua)?);
    h.insert("cookie", HeaderValue::from_str(cookie)?);
    h.insert("accept-language", HeaderValue::from_static("vi-VN,vi;q=0.9,en-US;q=0.6"));
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("pragma", HeaderValue::from_static("no-cache"));
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut tx, mut rx) = ws.split();
    let _ = ev.send(Event::Connected).await;

    let mut cipher_key: Option<String> = None;
    let mut req_id = 0u64;
    let mut ping = tokio::time::interval(Duration::from_millis(ping_ms));
    ping.tick().await; // first tick fires immediately

    loop {
        tokio::select! {
            frame = rx.next() => {
                let Some(frame) = frame else { return Ok(Stop::Closed(Error::WebSocket("socket ended".into()))) };
                let bytes = match frame? {
                    WsMsg::Binary(b) => b.to_vec(),
                    WsMsg::Text(t) => t.as_bytes().to_vec(),
                    WsMsg::Close(c) => {
                        let (code, reason) = c.map(|c| (u16::from(c.code), c.reason.to_string())).unwrap_or((0, String::new()));
                        return Ok(match code {
                            3000 => Stop::Final("another Zalo web session opened (duplicate connection)".into()),
                            3003 => Stop::Final("kicked by Zalo (3003)".into()),
                            _ => Stop::Closed(Error::WebSocket(format!("closed {code} {reason}"))),
                        });
                    }
                    _ => continue,
                };
                let Some(f) = parse_frame(&bytes)? else { continue };
                match (f.version, f.cmd, f.sub) {
                    (1, cmd::CIPHER_KEY, 1) => {
                        if let Some(k) = f.body["key"].as_str() {
                            cipher_key = Some(k.to_string());
                        }
                    }
                    (1, cmd::DUPLICATE, 0) => {
                        return Ok(Stop::Final("another Zalo web session opened (duplicate connection)".into()));
                    }
                    (_, c @ (cmd::USER_MSGS | cmd::GROUP_MSGS), 0) => {
                        let thread_type = ThreadType::from_group(c == cmd::GROUP_MSGS);
                        let data = decode_event_data(&f.body, cipher_key.as_deref())?;
                        let list = &data["data"][if thread_type.is_group() { "groupMsgs" } else { "msgs" }];
                        send_messages(ev, list, thread_type, uid, false).await;
                    }
                    (_, c @ (cmd::OLD_USER_MSGS | cmd::OLD_GROUP_MSGS), 1) => {
                        let thread_type = ThreadType::from_group(c == cmd::OLD_GROUP_MSGS);
                        let data = decode_event_data(&f.body, cipher_key.as_deref())?;
                        let list = &data["data"][if thread_type.is_group() { "groupMsgs" } else { "msgs" }];
                        // history holds the recall frames too: they take their message back, they are no message
                        send_messages(ev, list, thread_type, uid, true).await;
                    }
                    (_, c @ (cmd::USER_SEEN | cmd::GROUP_SEEN), 0) => {
                        // best effort: a bad receipt frame must not drop the connection
                        if let Ok(data) = decode_event_data(&f.body, cipher_key.as_deref()) {
                            let thread_type = ThreadType::from_group(c == cmd::GROUP_SEEN);
                            for seen in Seen::from_json(&data["data"], thread_type, uid) {
                                let _ = ev.send(Event::Seen(seen)).await;
                            }
                            for d in Delivered::from_json(&data["data"], thread_type, uid) {
                                let _ = ev.send(Event::Delivered(d)).await;
                            }
                        }
                    }
                    (_, cmd::REACTIONS, _) => {
                        // best effort, like receipts: a bad frame must not drop the connection
                        if let Ok(data) = decode_event_data(&f.body, cipher_key.as_deref()) {
                            for r in Reaction::from_json(&data["data"], uid) {
                                let _ = ev.send(Event::Reaction(r)).await;
                            }
                        }
                    }
                    _ => {} // typing / control: not decoded
                }
            }
            _ = ping.tick() => {
                if cipher_key.is_some() {
                    tx.send(WsMsg::Binary(encode_frame(1, cmd::PING, 1, &json!({"eventId": now_ms()})).into())).await?;
                }
            }
            _ = ev.closed() => return Ok(Stop::Final("listener dropped".into())),
            c = cmds.recv() => {
                match c {
                    None => return Ok(Stop::Final("stopped".into())),
                    Some(Cmd::RequestOld { thread_type, last_id }) => {
                        req_id += 1;
                        let body = json!({"first": true, "lastId": last_id.map(Value::String).unwrap_or(Value::Null), "preIds": [], "req_id": format!("req_{req_id}")});
                        let c = if thread_type.is_group() { cmd::OLD_GROUP_MSGS } else { cmd::OLD_USER_MSGS };
                        tx.send(WsMsg::Binary(encode_frame(1, c, 1, &body).into())).await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_backs_off_from_2s_to_60s_forever() {
        let p = ReconnectPolicy::default();
        let secs: Vec<u64> = (1..=8).map(|a| p.delay(a).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(p.delay(1000).as_secs(), 60, "no overflow far out");
        assert_eq!(p.max_attempts, None);
    }

    #[test]
    fn never_allows_no_retry_and_a_cap_counts_attempts() {
        assert_eq!(ReconnectPolicy::never().max_attempts, Some(0));
        let p = ReconnectPolicy { max_attempts: Some(2), initial_delay: Duration::from_millis(10), max_delay: Duration::from_millis(15) };
        assert_eq!((p.delay(1), p.delay(2), p.delay(3)), (Duration::from_millis(10), Duration::from_millis(15), Duration::from_millis(15)));
    }

    #[test]
    fn the_websocket_url_carries_the_web_client_version_and_type() {
        let u = ws_url("wss://ws1-msg.chat.zalo.me/?foo=1").unwrap();
        assert!(u.contains("foo=1") && u.contains("zpw_ver=685") && u.contains("zpw_type=30") && u.contains("&t="), "{u}");
    }

    #[tokio::test]
    async fn frames_become_events_messages_and_recalls_apart_and_history_as_a_page() {
        let (tx, mut rx) = mpsc::channel(16);
        let list = json!([
            {"msgId": "1", "uidFrom": "42", "idTo": "0", "ts": "1", "msgType": "webchat", "content": "hi"},
            {"msgId": "2", "uidFrom": "42", "idTo": "0", "content": {"globalMsgId": "1", "cliMsgId": "5", "deleteMsg": 3}},
            {"foo": 1}
        ]);
        send_messages(&tx, &list, ThreadType::User, "9", false).await;
        assert!(matches!(rx.recv().await, Some(Event::Message(m)) if m.msg_id == "1"));
        assert!(matches!(rx.recv().await, Some(Event::Undo(_))));
        send_messages(&tx, &list, ThreadType::Group, "9", true).await;
        assert!(matches!(rx.recv().await, Some(Event::Undo(_))));
        let Some(Event::History { thread_type, messages }) = rx.recv().await else { panic!() };
        assert_eq!((thread_type, messages.len()), (ThreadType::Group, 1));
        drop(tx);
        assert!(rx.recv().await.is_none());
    }
}
