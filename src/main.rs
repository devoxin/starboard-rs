use std::env::var;

use serenity::{all::{Cache, CacheHttp, ChannelId, Context, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateMessage, EditMessage, EventHandler, GatewayIntents, GuildChannel, GuildId, Message, MessageId, Reaction, UserId}, async_trait, Client};
use sqlx::{self, SqlitePool};

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
            let mut reference_content = format!("> Reply to {}\n", referenced.author.name);

            if referenced.content.is_empty() {
                reference_content += &format!("> [`No content, jump to message`]({})", referenced.link())
            } else if referenced.content.len() > 512 {
                let quote = &format!("{}...", &referenced.content[..509])
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<String>>()
                    .join("\n");

                reference_content += quote
            } else {
                reference_content += &referenced.content.lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<String>>()
                    .join("\n")
            }

            content += &reference_content
        }

        content += "\n\n";

        if message.content.len() > 1475 {
            content += &format!("{}...", &message.content[..1475]);
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

        // TODO: video attachments
        // TODO: hyperlink filtering
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

    async fn get_starboard_config(&self, cache: &Cache, guild_id: &GuildId) -> (Option<GuildChannel>, i8) {
        // TODO: Use query! macro as it validates queries.
        let (channel_id, min_stars) = match sqlx::query_as::<_, (i64, i8)>("SELECT channelid, minstars FROM configs WHERE guildid = ?")
            .bind(guild_id.get() as i64)
            .fetch_optional(&self.db)
            .await {
                Ok(Some((id, min_stars))) => (self.get_channel_from_guild_cache(cache, guild_id, &ChannelId::new(id as u64)), min_stars),
                Ok(None) => (self.find_starboard_channel(cache, guild_id), 1),
                Err(err) => {
                    eprintln!("Error in SQL: {err}");
                    return (None, 1);
                }
            };

        (channel_id, min_stars)
    }

    async fn get_starboard_message(&self, cache: impl CacheHttp, channel: &GuildChannel, message_id: MessageId) -> Option<Message> {
        match sqlx::query_as::<_, (String,)>("SELECT starid FROM starids WHERE msgid = ?")
            .bind(message_id.get() as i64)
            .fetch_optional(&self.db)
            .await {
                Ok(Some((id,))) => match channel.message(cache, MessageId::new(id.parse().expect("Failed to parse ID!"))).await {
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

    async fn check_reactions_and_delete(&self, ctx: &Context, reaction: &Reaction) {
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
        let Some(star_message) = self.get_starboard_message(&ctx.http, &channel, reaction.message_id).await else {
            return;
        };

        let Ok(message) = reaction.message(&ctx.http).await else {
            return;
        };

        let Ok(mut users) = reaction.users(&ctx.http, reaction.emoji.clone(), Some(100), None::<UserId>).await else {
            return;
        };

        users.retain(|u| !u.bot && u.id != message.author.id);

        if users.is_empty() || users.len() < min_stars.try_into().unwrap() {
            let _ = star_message.delete(&ctx.http).await;

            sqlx::query::<_>("DELETE FROM starids WHERE msgid = ?")
                .bind(reaction.message_id.get() as i64)
                .execute(&self.db)
                .await
                .expect("Failed to delete starboard entry from database!");
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

        let Ok(message) = reaction.message(&ctx.http).await else {
            return;
        };

        if message.content.is_empty() && message.attachments.is_empty() && (message.embeds.is_empty() || message.embeds[0].kind == Some("image".to_string())) {
            return;
        }

        let Ok(mut users) = reaction.users(&ctx.http, reaction.emoji.clone(), Some(100), None::<UserId>).await else {
            return;
        };

        users.retain(|u| !u.bot && u.id != message.author.id);

        if users.is_empty() || users.len() < min_stars.try_into().unwrap() {
            return;
        }

        if let Some(mut star_message) = self.get_starboard_message(&ctx.http, &channel, reaction.message_id).await {
            return star_message.edit(&ctx.http, EditMessage::new().content(format!("{} ⭐", users.len())))
                .await
                .expect("Failed to edit starboard message!");
        }

        let components = CreateActionRow::Buttons(vec![CreateButton::new_link(message.link()).label("Jump to Message")]);

        let to_send = CreateMessage::new()
            .content(format!("{} ⭐", users.len()))
            .embed(self.build_embed(&message))
            .components(vec![components]);

        if let Ok(starboard_message) = channel.send_message(&ctx.http, to_send).await {
            sqlx::query::<_>("INSERT INTO starids (msgid, starid) VALUES (?, ?)")
                .bind(message.id.get() as i64)
                .bind(starboard_message.id.get() as i64)
                .execute(&self.db)
                .await
                .expect("Failed to insert into database!");
        };
    }

    async fn reaction_remove(&self, ctx: Context, reaction: Reaction) {
        self.check_reactions_and_delete(&ctx, &reaction).await;
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

        sqlx::query::<_>("DELETE FROM starids WHERE msgid = ?")
                .bind(message_id.get() as i64)
                .execute(&self.db)
                .await
                .expect("Failed to delete starboard entry from database!");
    }

    async fn reaction_remove_emoji(&self, ctx: Context, reaction: Reaction) {
        self.check_reactions_and_delete(&ctx, &reaction).await;
    }
}

#[tokio::main]
async fn main() {
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
