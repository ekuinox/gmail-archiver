mod archive;
mod auth;
mod gmail;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Archive Gmail messages from a specific year into a zip file"
)]
struct Args {
    #[arg(long, help = "Year to archive, for example 2024")]
    year: i32,

    #[arg(
        long,
        default_value = "client_secret.json",
        value_name = "FILE",
        help = "Desktop OAuth client JSON downloaded from Google Cloud"
    )]
    oauth_client: PathBuf,

    #[arg(
        long,
        value_name = "FILE",
        help = "Token storage path. Defaults to the OS config directory"
    )]
    token_store: Option<PathBuf>,

    #[arg(
        long,
        value_name = "FILE",
        help = "Output zip file. Defaults to archives/gmail-<year>.zip"
    )]
    output: Option<PathBuf>,

    #[arg(
        long,
        value_name = "DIR",
        help = "Working directory for resumable downloads. Defaults to .gmail-archiver-work/<year>-<hash>"
    )]
    work_dir: Option<PathBuf>,

    #[arg(
        long,
        default_value_t = 500,
        help = "Messages per list request. Gmail API maximum is 500"
    )]
    page_size: u32,

    #[arg(
        long,
        value_name = "QUERY",
        help = "Extra Gmail search query, for example: label:work from:boss@example.com"
    )]
    query: Option<String>,

    #[arg(
        long,
        default_value_t = true,
        action = clap::ArgAction::Set,
        help = "Include spam and trash messages"
    )]
    include_spam_trash: bool,
}

#[derive(Debug, Clone)]
struct YearWindow {
    start_local: DateTime<Local>,
    end_local: DateTime<Local>,
}

impl YearWindow {
    fn for_year(year: i32) -> Result<Self> {
        let start_local = Local
            .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
            .single()
            .with_context(|| format!("Could not build the start timestamp for {year}"))?;

        let end_local = Local
            .with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0)
            .single()
            .with_context(|| format!("Could not build the start timestamp for {}", year + 1))?;

        Ok(Self {
            start_local,
            end_local,
        })
    }

    fn build_query(&self, extra_query: Option<&str>) -> String {
        let mut parts = vec![
            format!("after:{}", self.start_local.timestamp()),
            format!("before:{}", self.end_local.timestamp()),
        ];

        if let Some(extra_query) = extra_query {
            let trimmed = extra_query.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_owned());
            }
        }

        parts.join(" ")
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    if !(1970..=9999).contains(&args.year) {
        bail!("`--year` must be between 1970 and 9999");
    }

    let page_size = args.page_size.clamp(1, 500);
    let year_window = YearWindow::for_year(args.year)?;
    let query = year_window.build_query(args.query.as_deref());
    let output_path = args
        .output
        .unwrap_or_else(|| PathBuf::from("archives").join(format!("gmail-{}.zip", args.year)));
    let token_store = args.token_store.unwrap_or_else(default_token_store_path);
    let work_dir = args
        .work_dir
        .unwrap_or_else(|| default_work_dir_path(args.year, &query, args.include_spam_trash));

    println!(
        "Time range: {} to {}",
        year_window.start_local.to_rfc3339(),
        year_window.end_local.to_rfc3339()
    );
    println!("Gmail query: {query}");
    println!("Work directory: {}", work_dir.display());

    let http_client = auth::build_http_client()?;
    let authenticator = auth::Authenticator::from_client_secret_file(
        http_client.clone(),
        &args.oauth_client,
        token_store,
    )?;
    let mut gmail_client =
        gmail::GmailClient::new(http_client, authenticator, args.include_spam_trash);

    let summary = archive::write_archive(
        &mut gmail_client,
        archive::ArchiveRequest {
            year: args.year,
            query,
            start_local: year_window.start_local.to_rfc3339(),
            end_local: year_window.end_local.to_rfc3339(),
            output_path,
            work_dir,
            page_size,
            include_spam_trash: args.include_spam_trash,
        },
    )
    .await?;

    println!("Matched messages: {}", summary.message_count);
    println!("Reused staged messages: {}", summary.reused_messages);
    println!("Downloaded messages: {}", summary.downloaded_messages);
    println!("Wrote archive: {}", summary.output_path.display());
    Ok(())
}

fn default_token_store_path() -> PathBuf {
    if let Some(config_dir) = dirs::config_dir() {
        return config_dir.join("gmail-archiver").join("token.json");
    }

    PathBuf::from(".gmail-archiver").join("token.json")
}

fn default_work_dir_path(year: i32, query: &str, include_spam_trash: bool) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(year.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(query.as_bytes());
    hasher.update(b"\n");
    hasher.update(if include_spam_trash {
        "true".as_bytes()
    } else {
        "false".as_bytes()
    });

    let digest = hasher.finalize();
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    PathBuf::from(".gmail-archiver-work").join(format!("{year}-{hash}"))
}
