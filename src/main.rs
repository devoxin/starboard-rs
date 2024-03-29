use std::env::var;
use std::fmt::Write;

use dotenv::dotenv;
use serenity::{all::{Cache, CacheHttp, ChannelId, Context, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateMessage, EditMessage, EventHandler, GatewayIntents, GuildChannel, GuildId, HttpError, Message, MessageId, Reaction, UserId}, async_trait, Client};
use sqlx::SqlitePool;
use tokio::try_join;

struct Handler {
    db: SqlitePool,
}

impl Handler {
    async fn new() -> Handler {
        Handler {
            db: SqlitePool::connect("sqlite://star.db?mode=rwc").await.expect("Can't initialise SQL connection")
        }
    }

    fn build_embed(&self, message: &Message) -> CreateEmbed {
        let mut content = String::new();

        if let Some(referenced) = &message.referenced_message {
            writeln!(content, "> Reply to **{}**", referenced.author.name).unwrap();

            if referenced.content.is_empty() {
                writeln!(content, "> [`No content, jump to message`]({})\n", referenced.link()).unwrap();
            } else {
                // not sure about this to_owned tbh!
                let quote = if referenced.content.len() > 512 { format!("{}...", &referenced.content[..509]) } else { referenced.content.to_owned() };

                for line in quote.lines() {
                    writeln!(content, "> {line}").unwrap()
                }

                writeln!(content).unwrap();
            }
        }

        if message.content.len() > 1475 {
            write!(content, "{}...", &message.content[..1475]).unwrap();
        } else {
            content += &message.content;
        }

        let mut builder = CreateEmbed::new()
            .colour(0xFDD744)
            .author(CreateEmbedAuthor::new(&message.author.name).icon_url(message.author.face()))
            .description(content)
            .timestamp(message.timestamp);

        if let Some(image_url) = self.resolve_attachment(message) {
            builder = builder.image(image_url);
        }

        if let Some(video_url) = self.resolve_video(message) {
            builder = builder.field("\u{200b}", format!("[`Video Attachment`]({video_url})"), false)
        }

        // TODO: hyperlink filtering
        // TODO: tenor link embedding
        builder
    }

    fn resolve_attachment(&self, message: &Message) -> Option<String> {
        message.attachments.first()
            .and_then(|at| at.width.filter(|&w| w > 0).map(|_| at.url.to_string()))
            .or_else(|| {
                message.embeds.first().and_then(|em| {
                    em.image.as_ref()
                        .map(|img| img.url.to_string())
                        .or_else(|| em.thumbnail.as_ref().map(|thumb| thumb.url.to_string()))
                })
            })
    }

    fn resolve_video(&self, message: &Message) -> Option<String> {
        message.attachments.first()
            .and_then(|at| {
                at.content_type.as_ref()
                    .filter(|ct| ct.starts_with("video/"))
                    .map(|_| at.url.to_string())
            })
            .or_else(|| {
                message.embeds.first().and_then(|em| {
                    em.video.as_ref().map(|v| v.url.to_string())
                })
            })
    }

    fn find_starboard_channel(&self, cache: &Cache, guild_id: &GuildId) -> Option<GuildChannel> {
        let guild = cache.guild(guild_id)?;
        guild.channels.values().find(|channel| channel.name == "starboard").cloned()
    }

    fn get_channel_from_guild_cache(&self, cache: &Cache, guild_id: &GuildId, channel_id: &ChannelId) -> Option<GuildChannel> {
        let guild = cache.guild(guild_id)?;
        guild.channels.values().find(|channel| channel.id.eq(channel_id)).cloned()
    }

    fn get_channel_from_cache(&self, cache: &Cache, channel_id: &ChannelId) -> Option<GuildChannel> {
        cache.channel(channel_id).map(|channel| channel.clone())
    }

    async fn delete_starboard_entry(&self, message_id: MessageId) {
        sqlx::query::<_>("DELETE FROM starids WHERE msgid = ?")
            .bind(message_id.get() as i64)
            .execute(&self.db)
            .await
            .expect("Failed to delete starboard entry from database!");
    }

    async fn get_starboard_config(&self, cache: &Cache, guild_id: &GuildId) -> (Option<GuildChannel>, i8) {
        // TODO: Use query! macro as it validates queries.
        let (channel_id, min_stars) = match sqlx::query_as::<_, (i64, i8)>("SELECT channelid, minstars FROM configs WHERE guildid = ?")
            .bind(guild_id.get() as i64)
            .fetch_optional(&self.db)
            .await {
                Ok(Some((id, min_stars))) => {
                    let channel = if id == 0 { self.find_starboard_channel(cache, guild_id) } else { self.get_channel_from_guild_cache(cache, guild_id, &ChannelId::new(id as u64)) };
                    (channel, min_stars)
                }
                Ok(None) => (self.find_starboard_channel(cache, guild_id), 1),
                Err(err) => {
                    eprintln!("Error in SQL: {err}");
                    return (None, 1);
                }
            };

        (channel_id, min_stars)
    }

    async fn get_starboard_message(&self, cache: impl CacheHttp, channel: &GuildChannel, message_id: MessageId) -> Option<Message> {
        match sqlx::query_as::<_, (i64,)>("SELECT starid FROM starids WHERE msgid = ?")
            .bind(message_id.get() as i64)
            .fetch_optional(&self.db)
            .await {
                Ok(Some((id,))) => match channel.message(cache, MessageId::new(id as u64)).await {
                    Ok(message) => Some(message),
                    Err(_) => None
                },
                Ok(None) => None,
                Err(err) => {
                    eprintln!("Error in SQL: {err}");
                    None
                }
            }
    }

    async fn check_reactions_and_delete(&self, ctx: &Context, reaction: &Reaction, is_all: bool) {
        if !reaction.emoji.unicode_eq("⭐") {
            return;
        }

        let Some(guild_id) = reaction.guild_id else {
            return;
        };

        let (Some(channel), min_stars) = self.get_starboard_config(&ctx.cache, &guild_id).await else {
            return;
        };

        // If it's not starred, don't bother doing any additional handling.
        // I... also don't know why but it wants me to declare this mut.
        // Will figure out later. I'm just bug fixing atm.
        let Some(mut star_message) = self.get_starboard_message(&ctx.http, &channel, reaction.message_id).await else {
            return self.delete_starboard_entry(reaction.message_id).await;
        };

        // this is called by reaction_remove_emoji which is when an entire emoji is removed.
        // in that case, the count will be zero so we can short-circuit fetching the reaction count
        let mut reaction_count = 0;

        if !is_all {
            let Ok((message, users)) = try_join!(
                reaction.message(&ctx.http),
                reaction.users(&ctx.http, reaction.emoji.clone(), Some(100), None::<UserId>)
            ) else {
                return;
            };

            reaction_count = users.iter().filter(|u| !u.bot && u.id != message.author.id).count();
        }

        if reaction_count >= min_stars.try_into().unwrap() {
            match star_message.edit(&ctx.http, EditMessage::new().content(format!("{} ⭐", reaction_count))).await {
                Ok(()) => {},
                Err(serenity::Error::Http(HttpError::UnsuccessfulRequest(http_err))) => {
                    if http_err.status_code == 404 {
                        self.delete_starboard_entry(reaction.message_id).await;
                    }
                }
                Err(_) => {}
            }
        } else {
            let _ = star_message.delete(&ctx.http).await;
            self.delete_starboard_entry(reaction.message_id).await;
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        if !reaction.emoji.unicode_eq("⭐") {
            return;
        }

        let Some(guild_id) = reaction.guild_id else {
            return;
        };

        let (Some(channel), min_stars) = self.get_starboard_config(&ctx.cache, &guild_id).await else {
            return;
        };

        if channel.nsfw || reaction.channel_id == channel.id {
            return;
        }

        let Ok((message, users)) = try_join!(
            reaction.message(&ctx.http),
            reaction.users(&ctx.http, reaction.emoji.clone(), Some(100), None::<UserId>)
        ) else {
            return;
        };

        if message.content.is_empty() && message.attachments.is_empty() && (message.embeds.is_empty() || message.embeds[0].kind == Some("image".to_string())) {
            return;
        }

        let count = users.iter().filter(|u| !u.bot && u.id != message.author.id).count();

        if count < min_stars.try_into().unwrap() {
            return;
        }

        if let Some(mut star_message) = self.get_starboard_message(&ctx.http, &channel, reaction.message_id).await {
            match star_message.edit(&ctx.http, EditMessage::new().content(format!("{} ⭐", count))).await {
                Ok(()) => {},
                Err(serenity::Error::Http(HttpError::UnsuccessfulRequest(http_err))) => {
                    if http_err.status_code == 404 {
                        self.delete_starboard_entry(reaction.message_id).await;
                    }
                }
                Err(_) => {}
            }

            return;
        }

        let components = CreateActionRow::Buttons(vec![CreateButton::new_link(message.link()).label("Jump to Message")]);

        let to_send = CreateMessage::new()
            .content(format!("{} ⭐", count))
            .embed(self.build_embed(&message))
            .components(vec![components]);

        if let Ok(starboard_message) = channel.send_message(&ctx.http, to_send).await {
            sqlx::query::<_>("INSERT OR REPLACE INTO starids (msgid, starid) VALUES (?, ?)")
                .bind(message.id.get() as i64)
                .bind(starboard_message.id.get() as i64)
                .execute(&self.db)
                .await
                .expect("Failed to insert starboard entry into database!");
        };
    }

    async fn reaction_remove(&self, ctx: Context, reaction: Reaction) {
        self.check_reactions_and_delete(&ctx, &reaction, false).await;
    }

    async fn reaction_remove_emoji(&self, ctx: Context, reaction: Reaction) {
        self.check_reactions_and_delete(&ctx, &reaction, true).await;
    }

    async fn reaction_remove_all(&self, ctx: Context, channel_id: ChannelId, message_id: MessageId) {
        let (starboard_channel, _) = match self.get_channel_from_cache(&ctx.cache, &channel_id) {
            Some(channel) => match self.get_starboard_config(&ctx.cache, &channel.guild_id).await {
                (Some(channel), stars) => (channel, stars),
                (None, _) => return
            },
            None => return
        };

        let Some(starboard_message) = self.get_starboard_message(&ctx.http, &starboard_channel, message_id).await else {
            return;
        };

        // There shouldn't be any reactions left on this message so we delete it, and also from the database.
        let _ = starboard_message.delete(&ctx.http).await;
        self.delete_starboard_entry(message_id).await;
    }
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    let token = var("TOKEN").expect("Expected a token!");
    // probably don't need guild_messages, todo needs testing
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::GUILD_MESSAGE_REACTIONS;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler::new().await)
        .await
        .expect("Client instantiation failed!");

    if let Err(why) = client.start().await {
        eprintln!("Client error: {:?}", why);
    }
}
