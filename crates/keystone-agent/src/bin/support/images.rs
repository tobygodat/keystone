//! The images a scripted provider received with a message. With
//! `KEYSTONE_FAKE_IMAGE_RECEIPT` naming a file, each image is appended to it as one JSON
//! line, and each reply names the images it answered.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Appends `images` to the receipt file, and returns what a reply says about them.
pub fn record(images: &[Value]) -> String {
    if images.is_empty() {
        return String::new();
    }
    if let Some(path) = std::env::var_os("KEYSTONE_FAKE_IMAGE_RECEIPT")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let lines: String = images.iter().map(|image| format!("{image}\n")).collect();
        let _ = file.write_all(lines.as_bytes());
    }
    let named: Vec<String> = images
        .iter()
        .map(|image| {
            let kind = image["media_type"]
                .as_str()
                .or_else(|| image["path"].as_str()?.rsplit('.').next())
                .unwrap_or("?");
            let hash = image["sha256"].as_str().unwrap_or("missing");
            format!("{kind} {}", &hash[..hash.len().min(8)])
        })
        .collect();
    format!(" **Images:** {}", named.join(", "))
}
