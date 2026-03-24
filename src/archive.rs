use crate::gmail::GmailClient;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tokio::task::JoinSet;
use zip::{CompressionMethod, ZipWriter, write::FileOptions};

const STATE_VERSION: u32 = 1;

pub struct ArchiveRequest {
    pub year: i32,
    pub query: String,
    pub start_local: String,
    pub end_local: String,
    pub output_path: PathBuf,
    pub work_dir: PathBuf,
    pub page_size: u32,
    pub concurrency: usize,
    pub include_spam_trash: bool,
    pub remove_after_stage: bool,
}

pub struct ArchiveSummary {
    pub message_count: usize,
    pub reused_messages: usize,
    pub downloaded_messages: usize,
    pub removed_messages: usize,
    pub output_path: PathBuf,
}

pub async fn write_archive(
    gmail_client: &GmailClient,
    request: ArchiveRequest,
) -> Result<ArchiveSummary> {
    fs::create_dir_all(&request.work_dir).with_context(|| {
        format!(
            "Failed to create the work directory: {}",
            request.work_dir.display()
        )
    })?;

    let state_path = request.work_dir.join("state.json");
    let mut state = load_or_create_state(gmail_client, &request).await?;
    let messages_dir = request.work_dir.join("messages");
    fs::create_dir_all(&messages_dir).with_context(|| {
        format!(
            "Failed to create the staged messages directory: {}",
            messages_dir.display()
        )
    })?;

    let existing_staged_messages = state
        .message_ids
        .iter()
        .filter(|message_id| staged_message_path(&messages_dir, message_id).exists())
        .count();

    if existing_staged_messages > 0 {
        println!(
            "Found {existing_staged_messages} staged messages in {}, verifying them before resume.",
            request.work_dir.display()
        );
    } else {
        println!("No staged messages found, starting a fresh download.");
    }

    let (reused_messages, pending_download_ids) = verify_staged_messages(&state, &messages_dir)?;
    let downloaded_messages = download_missing_messages(
        gmail_client,
        &request,
        &messages_dir,
        &state_path,
        &mut state,
        pending_download_ids,
    )
    .await?;
    let removed_messages = trash_staged_messages(
        gmail_client,
        &request,
        &state_path,
        &mut state,
        &messages_dir,
    )
    .await?;

    let manifest = ArchiveManifest::from_state(&state);
    let manifest_json =
        serde_json::to_vec_pretty(&manifest).context("Failed to serialize manifest.json")?;
    let staged_manifest_path = request.work_dir.join("manifest.json");
    write_atomic(&staged_manifest_path, &manifest_json).with_context(|| {
        format!(
            "Failed to write the staged manifest file: {}",
            staged_manifest_path.display()
        )
    })?;

    if let Some(parent) = request.output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create the output directory: {}",
                    parent.display()
                )
            })?;
        }
    }

    let temp_output_path = temporary_path(&request.output_path, "part");
    create_zip_from_staged_files(
        &temp_output_path,
        &state,
        &messages_dir,
        &manifest_json,
        request.work_dir.as_path(),
    )?;
    move_into_place(&temp_output_path, &request.output_path)?;

    Ok(ArchiveSummary {
        message_count: state.message_ids.len(),
        reused_messages,
        downloaded_messages,
        removed_messages,
        output_path: request.output_path,
    })
}

async fn load_or_create_state(
    gmail_client: &GmailClient,
    request: &ArchiveRequest,
) -> Result<ArchiveState> {
    let state_path = request.work_dir.join("state.json");
    if state_path.exists() {
        let raw = fs::read(&state_path)
            .with_context(|| format!("Failed to read state.json: {}", state_path.display()))?;
        let state: ArchiveState =
            serde_json::from_slice(&raw).context("Failed to parse state.json")?;
        validate_state(&state, request)?;
        println!(
            "Loaded existing state with {} messages from {}.",
            state.message_ids.len(),
            state_path.display()
        );
        return Ok(state);
    }

    let message_ids = gmail_client
        .list_message_ids(&request.query, request.page_size)
        .await?;
    println!("Matched messages: {}", message_ids.len());

    let state = ArchiveState {
        version: STATE_VERSION,
        year: request.year,
        query: request.query.clone(),
        start_local: request.start_local.clone(),
        end_local: request.end_local.clone(),
        include_spam_trash: request.include_spam_trash,
        remove_after_stage: request.remove_after_stage,
        message_ids,
        message_sha256: BTreeMap::new(),
        removed_message_ids: BTreeSet::new(),
        created_at: Utc::now().to_rfc3339(),
    };

    persist_state(&state_path, &state)?;
    println!("Saved resume state to {}.", state_path.display());
    Ok(state)
}

fn verify_staged_messages(
    state: &ArchiveState,
    messages_dir: &Path,
) -> Result<(usize, Vec<String>)> {
    let mut reused_messages = 0usize;
    let mut pending_download_ids = Vec::new();

    for message_id in &state.message_ids {
        let staged_path = staged_message_path(messages_dir, message_id);
        if !staged_path.exists() {
            pending_download_ids.push(message_id.clone());
            continue;
        }

        match state.message_sha256.get(message_id) {
            Some(expected_hash) => {
                let actual_hash = sha256_hex_for_file(&staged_path).with_context(|| {
                    format!(
                        "Failed to hash the staged message file: {}",
                        staged_path.display()
                    )
                })?;
                if actual_hash == *expected_hash {
                    reused_messages += 1;
                } else {
                    println!(
                        "Hash mismatch for staged message {}, redownloading it.",
                        staged_path.display()
                    );
                    pending_download_ids.push(message_id.clone());
                }
            }
            None => {
                println!(
                    "Staged message {} has no saved hash, redownloading it.",
                    staged_path.display()
                );
                pending_download_ids.push(message_id.clone());
            }
        }
    }

    Ok((reused_messages, pending_download_ids))
}

async fn download_missing_messages(
    gmail_client: &GmailClient,
    request: &ArchiveRequest,
    messages_dir: &Path,
    state_path: &Path,
    state: &mut ArchiveState,
    pending_download_ids: Vec<String>,
) -> Result<usize> {
    if pending_download_ids.is_empty() {
        return Ok(0);
    }

    let mut downloaded_messages = 0usize;
    let mut in_flight = JoinSet::new();
    let mut pending_iter = pending_download_ids.into_iter();

    while in_flight.len() < request.concurrency {
        if let Some(message_id) = pending_iter.next() {
            spawn_download_task(&mut in_flight, gmail_client.clone(), message_id);
        } else {
            break;
        }
    }

    while let Some(join_result) = in_flight.join_next().await {
        let downloaded = join_result
            .context("A Gmail download task panicked")?
            .with_context(|| "A Gmail download task failed")?;

        let staged_path = staged_message_path(messages_dir, &downloaded.message_id);
        state
            .message_sha256
            .insert(downloaded.message_id.clone(), downloaded.sha256);
        persist_state(state_path, state)?;
        write_atomic(&staged_path, &downloaded.raw).with_context(|| {
            format!(
                "Failed to write the staged message file: {}",
                staged_path.display()
            )
        })?;

        downloaded_messages += 1;
        if downloaded_messages == 1
            || downloaded_messages == state.message_ids.len()
            || downloaded_messages % 25 == 0
        {
            println!("Downloaded {downloaded_messages} new messages");
        }

        if let Some(message_id) = pending_iter.next() {
            spawn_download_task(&mut in_flight, gmail_client.clone(), message_id);
        }
    }

    Ok(downloaded_messages)
}

async fn trash_staged_messages(
    gmail_client: &GmailClient,
    request: &ArchiveRequest,
    state_path: &Path,
    state: &mut ArchiveState,
    messages_dir: &Path,
) -> Result<usize> {
    if !request.remove_after_stage {
        return Ok(0);
    }

    let pending_remove_ids = state
        .message_ids
        .iter()
        .filter(|message_id| !state.removed_message_ids.contains(*message_id))
        .filter(|message_id| staged_message_path(messages_dir, message_id).exists())
        .cloned()
        .collect::<Vec<_>>();

    if pending_remove_ids.is_empty() {
        return Ok(0);
    }

    let mut removed_messages = 0usize;
    let mut in_flight = JoinSet::new();
    let mut pending_iter = pending_remove_ids.into_iter();

    while in_flight.len() < request.concurrency {
        if let Some(message_id) = pending_iter.next() {
            spawn_trash_task(&mut in_flight, gmail_client.clone(), message_id);
        } else {
            break;
        }
    }

    while let Some(join_result) = in_flight.join_next().await {
        let message_id = join_result
            .context("A Gmail trash task panicked")?
            .with_context(|| "A Gmail trash task failed")?;

        state.removed_message_ids.insert(message_id);
        persist_state(state_path, state)?;

        removed_messages += 1;
        if removed_messages == 1
            || removed_messages == state.message_ids.len()
            || removed_messages % 25 == 0
        {
            println!("Trashed {removed_messages} messages");
        }

        if let Some(message_id) = pending_iter.next() {
            spawn_trash_task(&mut in_flight, gmail_client.clone(), message_id);
        }
    }

    Ok(removed_messages)
}

fn spawn_download_task(
    join_set: &mut JoinSet<Result<DownloadedMessage>>,
    gmail_client: GmailClient,
    message_id: String,
) {
    join_set.spawn(async move {
        let message = gmail_client
            .get_raw_message(&message_id)
            .await
            .with_context(|| format!("Failed to fetch the message body: {message_id}"))?;
        let sha256 = sha256_hex(&message.raw);

        Ok(DownloadedMessage {
            message_id,
            raw: message.raw,
            sha256,
        })
    });
}

fn spawn_trash_task(
    join_set: &mut JoinSet<Result<String>>,
    gmail_client: GmailClient,
    message_id: String,
) {
    join_set.spawn(async move {
        gmail_client
            .trash_message(&message_id)
            .await
            .with_context(|| format!("Failed to move the message to Gmail trash: {message_id}"))?;
        Ok(message_id)
    });
}

fn validate_state(state: &ArchiveState, request: &ArchiveRequest) -> Result<()> {
    if state.version != STATE_VERSION {
        bail!(
            "The work directory contains an unsupported state version. Delete {} and try again.",
            request.work_dir.display()
        );
    }

    if state.year != request.year
        || state.query != request.query
        || state.start_local != request.start_local
        || state.end_local != request.end_local
        || state.include_spam_trash != request.include_spam_trash
        || state.remove_after_stage != request.remove_after_stage
    {
        bail!(
            "The work directory {} belongs to a different export configuration. Use another --work-dir or delete it before retrying.",
            request.work_dir.display()
        );
    }

    Ok(())
}

fn create_zip_from_staged_files(
    output_path: &Path,
    state: &ArchiveState,
    messages_dir: &Path,
    manifest_json: &[u8],
    work_dir: &Path,
) -> Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("Failed to create the zip file: {}", output_path.display()))?;
    let mut archive = ZipWriter::new(file);
    let options = FileOptions::default().compression_method(CompressionMethod::Deflated);

    println!("Building zip from staged files in {}.", work_dir.display());

    for (index, message_id) in state.message_ids.iter().enumerate() {
        let staged_path = staged_message_path(messages_dir, message_id);
        let expected_hash = state.message_sha256.get(message_id).with_context(|| {
            format!("Missing SHA-256 for staged message {message_id} in state.json")
        })?;
        archive
            .start_file(format!("messages/{message_id}.eml"), options)
            .context("Failed to start a zip entry")?;

        let mut staged_file = File::open(&staged_path).with_context(|| {
            format!(
                "Failed to open the staged message file: {}",
                staged_path.display()
            )
        })?;
        let mut buffer = Vec::new();
        staged_file
            .read_to_end(&mut buffer)
            .with_context(|| format!("Failed to read staged message: {}", staged_path.display()))?;
        let actual_hash = sha256_hex(&buffer);
        if actual_hash != *expected_hash {
            bail!(
                "SHA-256 mismatch for staged message {} while building the zip archive",
                staged_path.display()
            );
        }
        archive
            .write_all(&buffer)
            .context("Failed to write the message into the zip file")?;

        let current = index + 1;
        if current == 1 || current == state.message_ids.len() || current % 25 == 0 {
            println!("Packed {current} / {}", state.message_ids.len());
        }
    }

    archive
        .start_file("manifest.json", options)
        .context("Failed to add manifest.json to the zip file")?;
    archive
        .write_all(manifest_json)
        .context("Failed to write manifest.json to the zip file")?;

    archive
        .finish()
        .context("Failed to finalize the zip file")?;
    Ok(())
}

fn staged_message_path(messages_dir: &Path, message_id: &str) -> PathBuf {
    messages_dir.join(format!("{message_id}.eml"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }
    }

    let temp_path = temporary_path(path, "tmp");
    if temp_path.exists() {
        fs::remove_file(&temp_path).with_context(|| {
            format!(
                "Failed to remove a stale temporary file: {}",
                temp_path.display()
            )
        })?;
    }

    {
        let mut file = File::create(&temp_path).with_context(|| {
            format!(
                "Failed to create a temporary output file: {}",
                temp_path.display()
            )
        })?;
        file.write_all(bytes).with_context(|| {
            format!(
                "Failed to write a temporary output file: {}",
                temp_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "Failed to flush a temporary output file: {}",
                temp_path.display()
            )
        })?;
    }

    move_into_place(&temp_path, path)
}

fn move_into_place(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        fs::remove_file(to)
            .with_context(|| format!("Failed to replace existing file: {}", to.display()))?;
    }

    fs::rename(from, to).with_context(|| {
        format!(
            "Failed to move {} into place as {}",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

fn persist_state(state_path: &Path, state: &ArchiveState) -> Result<()> {
    let state_json = serde_json::to_vec_pretty(state).context("Failed to serialize state.json")?;
    write_atomic(state_path, &state_json)
        .with_context(|| format!("Failed to write state.json: {}", state_path.display()))
}

fn temporary_path(path: &Path, suffix: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_owned());
    path.with_file_name(format!("{file_name}.{suffix}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn sha256_hex_for_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open file for hashing: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read file for hashing: {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let digest = hasher.finalize();
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

#[derive(Debug, Serialize, Deserialize)]
struct ArchiveState {
    version: u32,
    year: i32,
    query: String,
    start_local: String,
    end_local: String,
    include_spam_trash: bool,
    #[serde(default)]
    remove_after_stage: bool,
    message_ids: Vec<String>,
    #[serde(default)]
    message_sha256: BTreeMap<String, String>,
    #[serde(default)]
    removed_message_ids: BTreeSet<String>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct ArchiveManifest {
    archived_at: String,
    created_at: String,
    date_range: DateRange,
    message_count: usize,
    note: &'static str,
    output_format: &'static str,
    query: String,
    year: i32,
}

impl ArchiveManifest {
    fn from_state(state: &ArchiveState) -> Self {
        Self {
            archived_at: Utc::now().to_rfc3339(),
            created_at: state.created_at.clone(),
            date_range: DateRange {
                start_local: state.start_local.clone(),
                end_local: state.end_local.clone(),
            },
            message_count: state.message_ids.len(),
            note: "messages/ contains Gmail API raw messages saved as .eml files",
            output_format: "zip + eml",
            query: state.query.clone(),
            year: state.year,
        }
    }
}

#[derive(Debug, Serialize)]
struct DateRange {
    start_local: String,
    end_local: String,
}

struct DownloadedMessage {
    message_id: String,
    raw: Vec<u8>,
    sha256: String,
}
