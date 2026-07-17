//! Talkyss headless bot SDK.
//!
//! Bots log in with a token (no GUI), join servers by invitation from a human
//! owner, and post to text channels they can see.
//!
//! ```ignore
//! let bot = talkyss_bot::Bot::login(&url, "bot_helper", "tbot_…").await?;
//! bot.send_message(&channel_id, "hello").await?;
//! ```

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use convex::{ConvexClient, FunctionResult, Value};
use futures::StreamExt;
use maplit::btreemap;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Bot {
    client: ConvexClient,
    session_token: String,
    pub bot_id: String,
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub conversation_id: String,
    pub name: String,
    pub channel_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub author_id: String,
    pub author_name: String,
    pub body: String,
    pub sent_at: f64,
    pub deleted: bool,
}

impl Bot {
    /// Headless login with bot username + one-time token from the desktop app.
    pub async fn login(
        deployment_url: &str,
        username: &str,
        token: &str,
    ) -> Result<Self> {
        let mut client = ConvexClient::new(deployment_url)
            .await
            .context("connect to Convex")?;

        let result = client
            .action(
                "bots:loginWithUsername",
                btreemap! {
                    "username".to_string() => Value::String(username.to_string()),
                    "token".to_string() => Value::String(token.to_string()),
                },
            )
            .await
            .map_err(|e| anyhow!("{e}"))?;

        let obj = match result {
            FunctionResult::Value(Value::Object(o)) => o,
            FunctionResult::ErrorMessage(e) => return Err(anyhow!(e)),
            FunctionResult::ConvexError(e) => return Err(anyhow!("{e:?}")),
            _ => return Err(anyhow!("unexpected login response")),
        };

        Ok(Self {
            client,
            session_token: value_str(&obj, "sessionToken")?,
            bot_id: value_str(&obj, "botId")?,
            username: value_str(&obj, "username")?,
            display_name: value_str(&obj, "displayName")?,
        })
    }

    pub fn session_token(&self) -> &str {
        &self.session_token
    }

    pub async fn send_message(&mut self, conversation_id: &str, body: &str) -> Result<()> {
        let result = self
            .client
            .mutation(
                "bots:sendMessage",
                btreemap! {
                    "sessionToken".to_string() => Value::String(self.session_token.clone()),
                    "conversationId".to_string() => Value::String(conversation_id.to_string()),
                    "body".to_string() => Value::String(body.to_string()),
                },
            )
            .await
            .map_err(|e| anyhow!("{e}"))?;
        match result {
            FunctionResult::Value(_) => Ok(()),
            FunctionResult::ErrorMessage(e) => Err(anyhow!(e)),
            FunctionResult::ConvexError(e) => Err(anyhow!("{e:?}")),
        }
    }

    pub async fn list_channels(&mut self, server_id: &str) -> Result<Vec<Channel>> {
        let result = self
            .client
            .query(
                "bots:listServerChannels",
                btreemap! {
                    "sessionToken".to_string() => Value::String(self.session_token.clone()),
                    "serverId".to_string() => Value::String(server_id.to_string()),
                },
            )
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let arr = match result {
            FunctionResult::Value(Value::Array(a)) => a,
            FunctionResult::ErrorMessage(e) => return Err(anyhow!(e)),
            FunctionResult::ConvexError(e) => return Err(anyhow!("{e:?}")),
            _ => return Err(anyhow!("unexpected channels response")),
        };
        Ok(arr
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(o) => Some(Channel {
                    conversation_id: value_str(&o, "conversationId").ok()?,
                    name: value_str(&o, "name").unwrap_or_else(|_| "channel".into()),
                    channel_type: value_str(&o, "channelType").unwrap_or_else(|_| "text".into()),
                }),
                _ => None,
            })
            .collect())
    }

    pub async fn recent_messages(
        &mut self,
        conversation_id: &str,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let result = self
            .client
            .query(
                "bots:listRecentMessages",
                btreemap! {
                    "sessionToken".to_string() => Value::String(self.session_token.clone()),
                    "conversationId".to_string() => Value::String(conversation_id.to_string()),
                    "limit".to_string() => Value::Float64(limit as f64),
                },
            )
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let arr = match result {
            FunctionResult::Value(Value::Array(a)) => a,
            FunctionResult::ErrorMessage(e) => return Err(anyhow!(e)),
            FunctionResult::ConvexError(e) => return Err(anyhow!("{e:?}")),
            _ => return Err(anyhow!("unexpected messages response")),
        };
        Ok(arr
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(o) => Some(Message {
                    id: value_str(&o, "id").ok()?,
                    author_id: value_str(&o, "authorId").ok()?,
                    author_name: value_str(&o, "authorName").unwrap_or_default(),
                    body: value_str(&o, "body").unwrap_or_default(),
                    sent_at: value_f64(&o, "sentAt"),
                    deleted: matches!(o.get("deleted"), Some(Value::Boolean(true))),
                }),
                _ => None,
            })
            .collect())
    }

    /// Subscribe to a channel and call `on_message` for each update batch.
    /// Blocks until the stream ends.
    pub async fn watch_messages<F, Fut>(
        &mut self,
        conversation_id: &str,
        mut on_batch: F,
    ) -> Result<()>
    where
        F: FnMut(Vec<Message>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut sub = self
            .client
            .subscribe(
                "bots:listRecentMessages",
                btreemap! {
                    "sessionToken".to_string() => Value::String(self.session_token.clone()),
                    "conversationId".to_string() => Value::String(conversation_id.to_string()),
                    "limit".to_string() => Value::Float64(30.0),
                },
            )
            .await
            .map_err(|e| anyhow!("{e}"))?;

        while let Some(result) = sub.next().await {
            let arr = match result {
                FunctionResult::Value(Value::Array(a)) => a,
                FunctionResult::ErrorMessage(e) => return Err(anyhow!(e)),
                _ => continue,
            };
            let msgs: Vec<Message> = arr
                .into_iter()
                .filter_map(|v| match v {
                    Value::Object(o) => Some(Message {
                        id: value_str(&o, "id").ok()?,
                        author_id: value_str(&o, "authorId").ok()?,
                        author_name: value_str(&o, "authorName").unwrap_or_default(),
                        body: value_str(&o, "body").unwrap_or_default(),
                        sent_at: value_f64(&o, "sentAt"),
                        deleted: matches!(o.get("deleted"), Some(Value::Boolean(true))),
                    }),
                    _ => None,
                })
                .collect();
            on_batch(msgs).await?;
        }
        Ok(())
    }
}

fn value_str(obj: &BTreeMap<String, Value>, key: &str) -> Result<String> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(anyhow!("missing string field {key}")),
    }
}

fn value_f64(obj: &BTreeMap<String, Value>, key: &str) -> f64 {
    match obj.get(key) {
        Some(Value::Float64(n)) => *n,
        Some(Value::Int64(n)) => *n as f64,
        _ => 0.0,
    }
}
