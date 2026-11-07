//! Log in to Zalo by QR code and save the credentials to a file.
//!
//! ```text
//! cargo run -p zca-rust --example login_qr -- zalo-creds.json
//! ```
//!
//! Scan the QR printed in the terminal with the Zalo app (Quét mã QR) and confirm on the
//! phone. The file holds your session cookies: keep it private (it is created readable by you
//! only) and never commit it.

use zca_rust::{qr, Client, QrEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: login_qr <file to save the credentials to>");
        std::process::exit(2);
    };

    let creds = Client::login_qr(|event| match event {
        QrEvent::Generated { png, expires_in, .. } => match qr::sample_png(&png) {
            Ok(matrix) => {
                println!("\n{}", qr::render(&matrix, 2));
                println!("Scan with the Zalo app (valid for {}s)...", expires_in.as_secs());
            }
            Err(e) => eprintln!("could not draw the QR ({e}); save the PNG and open it yourself"),
        },
        QrEvent::Scanned { display_name } => println!("Scanned by {display_name}: confirm the login on your phone..."),
        QrEvent::Expired => println!("The QR expired, a new one follows..."),
        _ => {}
    })
    .await?;

    // Log in once with the new cookies so the saved file holds what the server refreshed.
    let client = Client::from_credentials(creds).await?;
    // created with mode 0600; an existing, more open file is tightened too
    client.credentials()?.save_private(&path)?;

    println!("Logged in as uid {}. Credentials saved to {path}", client.uid());
    Ok(())
}
