use crate::gmail::{GmailClient, RawMessage};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Utc};
use serde::Serialize;
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};
use zip::{CompressionMethod, ZipWriter, write::FileOptions};

pub async fn write_archive(
    gmail_client: &mut GmailClient,
    year: i32,
    query: &str,
    start_local: String,
    end_local: String,
    output_path: &Path,
    message_ids: &[String],
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create the output directory: {}",
                    parent.display()
                )
            })?;
        }
    }

    let file = File::create(output_path)
        .with_context(|| format!("Failed to create the zip file: {}", output_path.display()))?;
    let mut archive = ZipWriter::new(file);
    let options = FileOptions::default().compression_method(CompressionMethod::Deflated);

    for (index, message_id) in message_ids.iter().enumerate() {
        let message = gmail_client
            .get_raw_message(message_id)
            .await
            .with_context(|| format!("Failed to fetch the message body: {message_id}"))?;
        let entry_name = build_entry_name(index + 1, &message);

        archive
            .start_file(entry_name, options)
            .context("Failed to start a zip entry")?;
        archive
            .write_all(&message.raw)
            .context("Failed to write the message into the zip file")?;

        let current = index + 1;
        if current == 1 || current == message_ids.len() || current % 25 == 0 {
            println!("Archived {current} / {}", message_ids.len());
        }
    }

    let manifest = ArchiveManifest {
        archived_at: Utc::now().to_rfc3339(),
        date_range: DateRange {
            start_local,
            end_local,
        },
        message_count: message_ids.len(),
        note: "messages/ contains Gmail API raw messages saved as .eml files",
        output_format: "zip + eml",
        query: query.to_owned(),
        year,
    };

    archive
        .start_file("manifest.json", options)
        .context("Failed to add manifest.json to the zip file")?;
    let manifest_json =
        serde_json::to_vec_pretty(&manifest).context("Failed to serialize manifest.json")?;
    archive
        .write_all(&manifest_json)
        .context("Failed to write manifest.json to the zip file")?;

    archive
        .finish()
        .context("Failed to finalize the zip file")?;
    Ok(())
}

fn build_entry_name(index: usize, message: &RawMessage) -> String {
    let timestamp = message
        .internal_date_ms
        .and_then(|milliseconds| Local.timestamp_millis_opt(milliseconds).single())
        .map(|datetime| datetime.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| "unknown-date".to_owned());

    format!("messages/{index:06}_{timestamp}_{}.eml", message.id)
}

#[derive(Debug, Serialize)]
struct ArchiveManifest {
    archived_at: String,
    date_range: DateRange,
    message_count: usize,
    note: &'static str,
    output_format: &'static str,
    query: String,
    year: i32,
}

#[derive(Debug, Serialize)]
struct DateRange {
    start_local: String,
    end_local: String,
}
