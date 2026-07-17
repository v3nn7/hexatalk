//! Minimal echo bot — replies to `!…` messages in a channel.
//!
//! ```bash
//! set CONVEX_URL=https://….convex.cloud
//! set BOT_USERNAME=bot_helper
//! set BOT_TOKEN=tbot_…
//! set CHANNEL_ID=j….
//! cargo run -p talkyss-bot --example echo
//! ```

use std::collections::HashSet;
use std::env;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let url = env::var("CONVEX_URL")?;
    let username = env::var("BOT_USERNAME")?;
    let token = env::var("BOT_TOKEN")?;
    let channel = env::var("CHANNEL_ID")?;

    let mut bot = talkyss_bot::Bot::login(&url, &username, &token).await?;
    println!("logged in as {} (@{})", bot.display_name, bot.username);
    println!("watching channel {channel}");

    let mut seen: HashSet<String> = HashSet::new();
    // Seed seen set so we don't echo history on startup.
    for m in bot.recent_messages(&channel, 30).await? {
        seen.insert(m.id);
    }

    loop {
        let msgs = bot.recent_messages(&channel, 20).await?;
        for m in msgs.into_iter().rev() {
            if seen.contains(&m.id) || m.deleted || m.author_id == bot.bot_id {
                continue;
            }
            seen.insert(m.id.clone());
            if let Some(rest) = m.body.strip_prefix('!') {
                let reply = format!("echo: {}", rest.trim());
                if let Err(e) = bot.send_message(&channel, &reply).await {
                    eprintln!("send failed: {e}");
                } else {
                    println!("← {} : {}", m.author_name, m.body);
                }
            }
        }
        if seen.len() > 400 {
            seen.clear();
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
