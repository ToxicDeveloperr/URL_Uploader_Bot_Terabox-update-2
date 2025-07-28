use std::{sync::Arc, time::Duration};

use anyhow::Result;
use async_read_progress::TokioAsyncReadProgressExt;
use dashmap::{DashMap, DashSet};
use futures::TryStreamExt;
use grammers_client::{
    button, reply_markup,
    types::{CallbackQuery, Chat, Message, User},
    Client, InputMessage, Update,
};
use log::{error, info, warn};
use reqwest::Url;
use scopeguard::defer;
use serde_json::Value;
use stream_cancel::{Trigger, Valved};
use tokio::sync::Mutex;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use mongodb::{Client as MongoClient, Collection, Database};
use regex::Regex;
use chrono::Utc;

use crate::command::{parse_command, Command};
use crate::models::{BatchPost, UploadedFile, PostStatus};

/// Bot is the main struct of the bot.
/// All the bot logic is implemented in this struct.
pub struct Bot {
    client: Client,
    me: User,
    http: reqwest::Client,
    locks: Arc<DashSet<i64>>,
    started_by: Arc<DashMap<i64, i64>>,
    triggers: Arc<DashMap<i64, Trigger>>,
    // MongoDB collections
    db: Database,
    batch_posts: Collection<BatchPost>,
    // Configuration
    input_channel_id: i64,
    store_channel_id: i64,
}

impl Bot {
    /// Create a new bot instance.
    pub async fn new(client: Client, mongo_uri: &str, input_channel_id: i64, store_channel_id: i64) -> Result<Arc<Self>> {
        let me = client.get_me().await?;
        
        // Connect to MongoDB
        let mongo_client = MongoClient::with_uri_str(mongo_uri).await?;
        let db = mongo_client.database("terabox_bot");
        let batch_posts = db.collection::<BatchPost>("batch_posts");
        
        // Create TTL index for automatic cleanup
        use mongodb::options::IndexOptions;
        use mongodb::IndexModel;
        let index = IndexModel::builder()
            .keys(mongodb::bson::doc! { "expires_at": 1 })
            .options(IndexOptions::builder().expire_after(Duration::from_secs(0)).build())
            .build();
        batch_posts.create_index(index, None).await?;
        
        Ok(Arc::new(Self {
            client,
            me,
            http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36")
            .build()?,
            locks: Arc::new(DashSet::new()),
            started_by: Arc::new(DashMap::new()),
            triggers: Arc::new(DashMap::new()),
            db,
            batch_posts,
            input_channel_id,
            store_channel_id,
        }))
    }

    /// Check if URL is from Terabox domain
    fn is_terabox_url(&self, url: &Url) -> bool {
        if let Some(domain) = url.domain() {
            let domain = domain.to_lowercase();
            domain.contains("terabox") || 
            domain.contains("1024tera") || 
            domain.contains("4funbox") ||
            domain.contains("mirrobox") ||
            domain.contains("nephobox") ||
            domain.contains("terasharelink") ||
            domain.contains("terafileshare")
        } else {
            false
        }
    }

    /// Get final stream link for Terabox URLs
    async fn get_final_stream_link(&self, terabox_url: &str) -> Result<Option<String>> {
        let api_url = "https://teradl.in/api/teradl.php";
        
        let response = self.http
            .get(api_url)
            .query(&[("url", terabox_url)])
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36")
            .header("Accept", "*/*")
            .header("Referer", "https://teradl.in/")
            .header("Accept-Encoding", "gzip, deflate, br, zstd")
            .header("Accept-Language", "en-US,en;q=0.5")
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-GPC", "1")
            .send()
            .await?;

        if response.status().is_success() {
            let data: Value = response.json().await?;
            
            if let Some(success) = data.get("success") {
                if success.as_bool().unwrap_or(false) {
                    if let Some(proxy_url) = data.get("proxy_url") {
                        if let Some(url_str) = proxy_url.as_str() {
                            return Ok(Some(url_str.to_string()));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Run the bot.
    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, exiting");
                    break;
                }

                Ok(update) = self.client.next_update() => {
                    let self_ = self.clone();

                    // Spawn a new task to handle the update
                    tokio::spawn(async move {
                        if let Err(err) = self_.handle_update(update).await {
                            error!("Error handling update: {}", err);
                        }
                    });
                }
            }
        }
    }

    /// Update handler.
    async fn handle_update(&self, update: Update) -> Result<()> {
        // NOTE: no ; here, so result is returned
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    /// Message handler.
    ///
    /// Ensures the message is from a user or a group, and then parses the command.
    /// If the command is not recognized, it will try to parse the message as a URL.
    async fn handle_message(&self, msg: Message) -> Result<()> {
        // Check if this message is from the input channel
        if msg.chat().id() == self.input_channel_id {
            return self.handle_input_channel_message(msg).await;
        }

        // Ensure the message chat is a user or a group
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {}
            _ => return Ok(()),
        };

        // Parse the command
        let command = parse_command(msg.text());
        if let Some(command) = command {
            // Ensure the command is for this bot
            if let Some(via) = &command.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Ignoring command for unknown bot: {}", via);
                    return Ok(());
                }
            }

            // There is a chance that there are multiple bots listening
            // to /start commands in a group, so we handle commands
            // only if they are sent explicitly to this bot.
            if let Chat::Group(_) = msg.chat() {
                if command.name == "start" && command.via.is_none() {
                    return Ok(());
                }
            }

            // Handle the command
            info!("Received command: {:?}", command);
            match command.name.as_str() {
                "start" => {
                    return self.handle_start(msg).await;
                }
                "upload" => {
                    return self.handle_upload(msg, command).await;
                }
                _ => {}
            }
        }

        if let Chat::User(_) = msg.chat() {
            // If the message is not a command, try to parse it as a URL
            if let Ok(url) = Url::parse(msg.text()) {
                return self.handle_url(msg, url).await;
            }
        }

        Ok(())
    }

    /// Handle the /start command.
    /// This command is sent when the user starts a conversation with the bot.
    /// It will reply with a welcome message.
    async fn handle_start(&self, msg: Message) -> Result<()> {
        msg.reply(InputMessage::html(
            "📁 <b>Hi! Need a file uploaded? Just send the link!</b>\n\
            In groups, use <code>/upload &lt;url&gt;</code>\n\
            \n\
            🌟 <b>Features:</b>\n\
            \u{2022} Free & fast\n\
            \u{2022} <a href=\"https://github.com/HerMan-Official/URL_Uploader_Bot_Telegram\">Open source</a>\n\
            \u{2022} Uploads files up to 2GB\n\
            \u{2022} Redirect-friendly\n\
            \u{2022} <b>Terabox support</b>",
        ))
        .await?;
        Ok(())
    }

    /// Handle the /upload command.
    /// This command should be used in groups to upload a file.
    async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
        // If the argument is not specified, reply with an error
        let url = match cmd.arg {
            Some(url) => url,
            None => {
                msg.reply("Please specify a URL").await?;
                return Ok(());
            }
        };

        // Parse the URL
        let url = match Url::parse(&url) {
            Ok(url) => url,
            Err(err) => {
                msg.reply(format!("Invalid URL: {}", err)).await?;
                return Ok(());
            }
        };

        self.handle_url(msg, url).await
    }

    /// Handle a URL.
    /// This function will download the file and upload it to Telegram.
    async fn handle_url(&self, msg: Message, url: Url) -> Result<()> {
        let sender = match msg.sender() {
            Some(sender) => sender,
            None => return Ok(()),
        };

        // Lock the chat to prevent multiple uploads at the same time
        info!("Locking chat {}", msg.chat().id());
        let _lock = self.locks.insert(msg.chat().id());
        if !_lock {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.")
                .await?;
            return Ok(());
        }
        self.started_by.insert(msg.chat().id(), sender.id());

        // Deferred unlock
        defer! {
            info!("Unlocking chat {}", msg.chat().id());
            self.locks.remove(&msg.chat().id());
            self.started_by.remove(&msg.chat().id());
        };

        // Check if it's a Terabox URL and get the proxy URL if needed
        let download_url = if self.is_terabox_url(&url) {
            info!("Detected Terabox URL: {}", url);
            msg.reply("🔄 Processing Terabox link...").await?;
            
            match self.get_final_stream_link(url.as_str()).await {
                Ok(Some(proxy_url)) => {
                    info!("Got proxy URL for Terabox: {}", proxy_url);
                    match Url::parse(&proxy_url) {
                        Ok(parsed_url) => parsed_url,
                        Err(err) => {
                            msg.reply(format!("❌ Failed to parse proxy URL: {}", err)).await?;
                            return Ok(());
                        }
                    }
                }
                Ok(None) => {
                    msg.reply("❌ Failed to get download link from Terabox. The file might be private or the link is invalid.").await?;
                    return Ok(());
                }
                Err(err) => {
                    error!("Error getting Terabox proxy URL: {}", err);
                    msg.reply("❌ Failed to process Terabox link. Please try again.").await?;
                    return Ok(());
                }
            }
        } else {
            // Use the original URL for non-Terabox links
            url
        };

        info!("Downloading file from {}", download_url);
        let response = self.http.get(download_url).send().await?;

        // Get the file name and size
        let length = response.content_length().unwrap_or_default() as usize;
        let name = match response
            .headers()
            .get("content-disposition")
            .and_then(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| {
                        value
                            .split(';')
                            .map(|value| value.trim())
                            .find(|value| value.starts_with("filename="))
                    })
                    .map(|value| value.trim_start_matches("filename="))
                    .map(|value| value.trim_matches('"'))
            }) {
            Some(name) => name.to_string(),
            None => response
                .url()
                .path_segments()
                .and_then(|segments| segments.last())
                .and_then(|name| {
                    if name.contains('.') {
                        Some(name.to_string())
                    } else {
                        // guess the extension from the content type
                        response
                            .headers()
                            .get("content-type")
                            .and_then(|value| value.to_str().ok())
                            .and_then(mime_guess::get_mime_extensions_str)
                            .and_then(|ext| ext.first())
                            .map(|ext| format!("{}.{}", name, ext))
                    }
                })
                .unwrap_or("file.bin".to_string())
                .to_string(),
        };
        let name = percent_encoding::percent_decode_str(&name)
            .decode_utf8()?
            .to_string();
        let is_video = response
            .headers()
            .get("content-type")
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .map(|value| value.starts_with("video/mp4"))
                    .unwrap_or_default()
            })
            .unwrap_or_default()
            || name.to_lowercase().ends_with(".mp4");
        info!("File {} ({} bytes, video: {})", name, length, is_video);

        // File is empty
        if length == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }

        // File is too large
        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        // Wrap the response stream in a valved stream
        let (trigger, stream) = Valved::new(
            response
                .bytes_stream()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
        );
        self.triggers.insert(msg.chat().id(), trigger);

        // Deferred trigger removal
        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        // Reply markup buttons
        let reply_markup = Arc::new(reply_markup::inline(vec![vec![button::inline(
            "⛔ Cancel",
            "cancel",
        )]]));

        // Send status message
        let status = Arc::new(Mutex::new(
            msg.reply(
                InputMessage::html(format!("🚀 Starting upload of <code>{}</code>...", name))
                    .reply_markup(reply_markup.clone().as_ref()),
            )
            .await?,
        ));

        let mut stream = stream
            .into_async_read()
            .compat()
            // Report progress every 3 seconds
            .report_progress(Duration::from_secs(3), |progress| {
                let status = status.clone();
                let name = name.clone();
                let reply_markup = reply_markup.clone();
                tokio::spawn(async move {
                    status
                        .lock()
                        .await
                        .edit(
                            InputMessage::html(format!(
                                "⏳ Uploading <code>{}</code> <b>({:.2}%)</b>\n\
                            <i>{} / {}</i>",
                                name,
                                progress as f64 / length as f64 * 100.0,
                                bytesize::to_string(progress as u64, true),
                                bytesize::to_string(length as u64, true),
                            ))
                            .reply_markup(reply_markup.as_ref()),
                        )
                        .await
                        .ok();
                });
            });

        // Upload the file
        let start_time = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, length, name.clone())
            .await?;

        // Calculate upload time
        let elapsed = chrono::Utc::now() - start_time;
        info!("Uploaded file {} ({} bytes) in {}", name, length, elapsed);

        // Send file
        let mut input_msg = InputMessage::html(format!(
            "Uploaded in <b>{:.2} secs</b>",
            elapsed.num_milliseconds() as f64 / 1000.0
        ));
        input_msg = input_msg.document(file);
        if is_video {
            input_msg = input_msg.attribute(grammers_client::types::Attribute::Video {
                supports_streaming: false,
                duration: Duration::ZERO,
                w: 0,
                h: 0,
                round_message: false,
            });
        }
        msg.reply(input_msg).await?;

        // Delete status message
        status.lock().await.delete().await?;

        Ok(())
    }

    /// Callback query handler.
    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        match query.data() {
            b"cancel" => self.handle_cancel(query).await,
            _ => Ok(()),
        }
    }

    /// Handle the cancel button.
    async fn handle_cancel(&self, query: CallbackQuery) -> Result<()> {
        let started_by_user_id = match self.started_by.get(&query.chat().id()) {
            Some(id) => *id,
            None => return Ok(()),
        };

        if started_by_user_id != query.sender().id() {
            info!(
                "Some genius with ID {} tried to cancel another user's upload in chat {}",
                query.sender().id(),
                query.chat().id()
            );

            query
                .answer()
                .alert("⚠️ You can't cancel another user's upload")
                .cache_time(Duration::ZERO)
                .send()
                .await?;

            return Ok(());
        }

        if let Some((chat_id, trigger)) = self.triggers.remove(&query.chat().id()) {
            info!("Cancelling upload in chat {}", chat_id);
            drop(trigger);
            self.started_by.remove(&chat_id);

            query
                .load_message()
                .await?
                .edit("⛔ Upload cancelled")
                .await?;

            query.answer().send().await?;
        }
        Ok(())
    }

    /// Handle input channel messages for batch processing
    async fn handle_input_channel_message(&self, msg: Message) -> Result<()> {
        let message_text = msg.text();
        let post_id = msg.id();
        let channel_id = msg.chat().id();
        
        info!("Processing batch message from input channel: post_id={}, channel_id={}", post_id, channel_id);
        
        // Extract Terabox links from the message
        let detected_links = self.extract_terabox_links(message_text);
        
        if detected_links.is_empty() {
            info!("No Terabox links found in message {}", post_id);
            return Ok(());
        }
        
        info!("Found {} Terabox links in message {}", detected_links.len(), post_id);
        
        // Create batch post entry in MongoDB
        let batch_post = BatchPost::new(post_id, channel_id, message_text.to_string(), detected_links.clone());
        self.batch_posts.insert_one(&batch_post, None).await?;
        
        // Start processing links with timeout
        let self_clone = Arc::clone(&self);
        let msg_clone = msg.clone();
        tokio::spawn(async move {
            // Set up 6-hour timeout
            let timeout_duration = Duration::from_secs(6 * 60 * 60); // 6 hours
            
            let result = tokio::time::timeout(
                timeout_duration,
                self_clone.process_batch_links(post_id, channel_id, detected_links, msg_clone)
            ).await;
            
            match result {
                Ok(Ok(())) => {
                    info!("Batch processing completed successfully for post {}", post_id);
                }
                Ok(Err(e)) => {
                    error!("Error during batch processing for post {}: {}", post_id, e);
                    if let Err(e) = self_clone.mark_batch_failed(post_id, channel_id).await {
                        error!("Failed to mark batch as failed: {}", e);
                    }
                }
                Err(_) => {
                    warn!("Batch processing timed out for post {}", post_id);
                    if let Err(e) = self_clone.mark_batch_timeout(post_id, channel_id).await {
                        error!("Failed to mark batch as timeout: {}", e);
                    }
                }
            }
        });
        
        Ok(())
    }
    
    /// Extract Terabox links from message text
    fn extract_terabox_links(&self, text: &str) -> Vec<String> {
        let url_regex = Regex::new(r"https?://[^\s]+").unwrap();
        let mut terabox_links = Vec::new();
        
        for cap in url_regex.find_iter(text) {
            let url_str = cap.as_str();
            if let Ok(url) = Url::parse(url_str) {
                if self.is_terabox_url(&url) {
                    terabox_links.push(url_str.to_string());
                }
            }
        }
        
        terabox_links
    }
    
    /// Process batch of links sequentially
    async fn process_batch_links(&self, post_id: i32, channel_id: i64, links: Vec<String>, original_msg: Message) -> Result<()> {
        for (index, link) in links.iter().enumerate() {
            info!("Processing link {}/{} for post {}: {}", index + 1, links.len(), post_id, link);
            
            match self.process_single_terabox_link(link, post_id, channel_id).await {
                Ok(uploaded_file) => {
                    info!("Successfully processed link {}: {}", index + 1, uploaded_file.file_name);
                }
                Err(e) => {
                    error!("Failed to process link {}: {} - Error: {}", index + 1, link, e);
                    // Continue with next link instead of failing entire batch
                    continue;
                }
            }
        }
        
        // Check if all links are processed and send completion message
        if let Ok(batch_post) = self.get_batch_post(post_id, channel_id).await {
            if matches!(batch_post.status, PostStatus::Done) {
                self.send_completion_message(&original_msg, &batch_post).await?;
            }
        }
        
        Ok(())
    }
    
    /// Process a single Terabox link and upload to store channel
    async fn process_single_terabox_link(&self, terabox_url: &str, post_id: i32, channel_id: i64) -> Result<UploadedFile> {
        // Get final stream link
        let download_url = match self.get_final_stream_link(terabox_url).await? {
            Some(url) => url,
            None => return Err(anyhow::anyhow!("Failed to get download link from Terabox")),
        };
        
        // Download file
        let response = self.http.get(&download_url).send().await?;
        let length = response.content_length().unwrap_or_default() as usize;
        
        // Get file name
        let name = self.extract_filename_from_response(&response).unwrap_or_else(|| "unknown_file.bin".to_string());
        
        // Check file constraints
        if length == 0 {
            return Err(anyhow::anyhow!("File is empty"));
        }
        if length > 2 * 1024 * 1024 * 1024 {
            return Err(anyhow::anyhow!("File is too large"));
        }
        
        // Upload to Telegram
        let stream = response.bytes_stream().map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err));
        let mut async_read = stream.into_async_read().compat();
        
        let file = self.client.upload_stream(&mut async_read, length, name.clone()).await?;
        
        // Send to store channel
        let store_channel = grammers_client::types::Chat::from_id(self.store_channel_id);
        let mut input_msg = InputMessage::text(format!("File: {}", name)).document(file);
        
        // Set video attribute if it's a video file
        if name.to_lowercase().ends_with(".mp4") {
            input_msg = input_msg.attribute(grammers_client::types::Attribute::Video {
                supports_streaming: false,
                duration: Duration::ZERO,
                w: 0,
                h: 0,
                round_message: false,
            });
        }
        
        let sent_msg = self.client.send_message(&store_channel, input_msg).await?;
        let file_id = self.extract_file_id_from_message(&sent_msg)?;
        
        // Create uploaded file record
        let uploaded_file = UploadedFile {
            original_url: terabox_url.to_string(),
            file_name: name,
            telegram_file_id: file_id,
            store_channel_msg_id: sent_msg.id(),
        };
        
        // Update MongoDB with uploaded file
        self.update_batch_post_with_upload(post_id, channel_id, uploaded_file.clone()).await?;
        
        Ok(uploaded_file)
    }
    
    /// Extract filename from HTTP response
    fn extract_filename_from_response(&self, response: &reqwest::Response) -> Option<String> {
        response
            .headers()
            .get("content-disposition")
            .and_then(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| {
                        value
                            .split(';')
                            .map(|value| value.trim())
                            .find(|value| value.starts_with("filename="))
                    })
                    .map(|value| value.trim_start_matches("filename="))
                    .map(|value| value.trim_matches('"'))
            })
            .map(|name| name.to_string())
            .or_else(|| {
                response
                    .url()
                    .path_segments()
                    .and_then(|segments| segments.last())
                    .map(|name| name.to_string())
            })
    }
    
    /// Extract file ID from sent message
    fn extract_file_id_from_message(&self, msg: &Message) -> Result<String> {
        if let Some(media) = msg.media() {
            match media {
                grammers_client::types::Media::Document(doc) => {
                    Ok(format!("{:?}", doc.id()))
                }
                grammers_client::types::Media::Video(video) => {
                    Ok(format!("{:?}", video.id()))
                }
                _ => Err(anyhow::anyhow!("Unsupported media type")),
            }
        } else {
            Err(anyhow::anyhow!("No media found in message"))
        }
    }
    
    /// Update batch post with uploaded file
    async fn update_batch_post_with_upload(&self, post_id: i32, channel_id: i64, uploaded_file: UploadedFile) -> Result<()> {
        use mongodb::bson::doc;
        
        let filter = doc! { "post_id": post_id, "channel_id": channel_id };
        let update = doc! {
            "$push": { "uploaded": mongodb::bson::to_bson(&uploaded_file)? }
        };
        
        self.batch_posts.update_one(filter.clone(), update, None).await?;
        
        // Check if all links are uploaded and update status
        if let Ok(batch_post) = self.get_batch_post(post_id, channel_id).await {
            if batch_post.uploaded.len() == batch_post.detected_links.len() {
                let status_update = doc! {
                    "$set": { "status": "done" }
                };
                self.batch_posts.update_one(filter, status_update, None).await?;
            }
        }
        
        Ok(())
    }
    
    /// Get batch post from MongoDB
    async fn get_batch_post(&self, post_id: i32, channel_id: i64) -> Result<BatchPost> {
        use mongodb::bson::doc;
        
        let filter = doc! { "post_id": post_id, "channel_id": channel_id };
        match self.batch_posts.find_one(filter, None).await? {
            Some(batch_post) => Ok(batch_post),
            None => Err(anyhow::anyhow!("Batch post not found")),
        }
    }
    
    /// Mark batch as failed
    async fn mark_batch_failed(&self, post_id: i32, channel_id: i64) -> Result<()> {
        use mongodb::bson::doc;
        
        let filter = doc! { "post_id": post_id, "channel_id": channel_id };
        let update = doc! { "$set": { "status": "failed" } };
        
        self.batch_posts.update_one(filter, update, None).await?;
        Ok(())
    }
    
    /// Mark batch as timeout
    async fn mark_batch_timeout(&self, post_id: i32, channel_id: i64) -> Result<()> {
        use mongodb::bson::doc;
        
        let filter = doc! { "post_id": post_id, "channel_id": channel_id };
        let update = doc! { "$set": { "status": "timeout" } };
        
        self.batch_posts.update_one(filter, update, None).await?;
        Ok(())
    }
    
    /// Send completion message to input channel
    async fn send_completion_message(&self, original_msg: &Message, batch_post: &BatchPost) -> Result<()> {
        let mut message_lines = vec!["/store_file_id".to_string()];
        
        for uploaded_file in &batch_post.uploaded {
            message_lines.push(uploaded_file.telegram_file_id.clone());
        }
        
        let completion_message = message_lines.join("\n");
        
        original_msg.reply(InputMessage::text(completion_message)).await?;
        
        info!("Sent completion message for batch post {}", batch_post.post_id);
        Ok(())
    }
}
