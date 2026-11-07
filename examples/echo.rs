//! Listen to your Zalo account and print every message. **Read-only**: it never replies or
//! sends anything.
//!
//! ```text
//! cargo run -p zca-rust --example echo -- zalo-creds.json
//! ```
//!
//! Get the file from the `login_qr` example. Zalo allows one web listener per account, so
//! this stops with a message when you open Zalo Web elsewhere.

use zca_rust::{Client, Content, Credentials, Event, ZaloMessage};

fn describe(m: &ZaloMessage) -> String {
    match &m.content {
        Content::Text(t) => t.clone(),
        Content::Sticker(_) => "[sticker]".into(),
        Content::Gif(g) => format!("[gif {}x{}]", g.width.unwrap_or(0), g.height.unwrap_or(0)),
        Content::Attachment(a) => format!("[{:?}] {}", a.kind, a.card.title.as_deref().unwrap_or("")),
        Content::Link(c) => format!("[link] {}", c.href.as_deref().unwrap_or("")),
        Content::Call { .. } => "[call]".into(),
        _ => "[other]".into(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: echo <credentials file from the login_qr example>");
        std::process::exit(2);
    };
    let creds = Credentials::load(&path)?;
    let client = Client::from_credentials(creds).await?;
    println!("Listening as uid {} (Ctrl+C to stop)...", client.uid());

    let mut events = client.listen()?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    Event::Connected => println!("* connected"),
                    Event::Message(m) | Event::GroupMessage(m) => {
                        let who = if m.is_self { "me" } else if m.d_name.is_empty() { &m.uid_from } else { &m.d_name };
                        println!("[{:?} {}] {who}: {}", m.thread_type, m.thread_id, describe(&m));
                    }
                    Event::Reaction(r) => println!("* reaction {} on {} in {}", r.icon.emoji(), r.msg_id, r.thread_id),
                    Event::Undo(u) => println!("* message recalled in {}", u.thread_id),
                    Event::Seen(s) => println!("* {} saw messages in {}", s.reader_id, s.thread_id),
                    Event::Disconnected { reason } => println!("* disconnected: {reason}"),
                    Event::Reconnecting { attempt, delay } => println!("* reconnecting (#{attempt}) in {delay:?}"),
                    Event::Stopped { reason } => {
                        println!("* stopped: {reason}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
