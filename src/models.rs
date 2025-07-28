use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedFile {
    pub original_url: String,
    pub file_name: String,
    pub telegram_file_id: String,
    pub store_channel_msg_id: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PostStatus {
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "timeout")]
    Timeout,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchPost {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub post_id: i32,
    pub channel_id: i64,
    pub message_text: String,
    pub detected_links: Vec<String>,
    pub uploaded: Vec<UploadedFile>,
    pub status: PostStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl BatchPost {
    pub fn new(post_id: i32, channel_id: i64, message_text: String, detected_links: Vec<String>) -> Self {
        let now = Utc::now();
        let expires_at = now + chrono::Duration::hours(6);
        
        Self {
            id: None,
            post_id,
            channel_id,
            message_text,
            detected_links,
            uploaded: Vec::new(),
            status: PostStatus::InProgress,
            created_at: now,
            expires_at,
        }
    }
    
    pub fn add_uploaded_file(&mut self, uploaded_file: UploadedFile) {
        self.uploaded.push(uploaded_file);
        
        // Check if all links are uploaded
        if self.uploaded.len() == self.detected_links.len() {
            self.status = PostStatus::Done;
        }
    }
    
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
    
    pub fn mark_timeout(&mut self) {
        self.status = PostStatus::Timeout;
    }
    
    pub fn mark_failed(&mut self) {
        self.status = PostStatus::Failed;
    }
}
