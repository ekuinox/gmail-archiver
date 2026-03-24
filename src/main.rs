mod archive;
mod auth;
mod gmail;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Archive Gmail messages from a specific year or year range into zip files"
)]
struct Args {
    #[arg(
        long,
        value_name = "YEAR_OR_RANGE",
        help = "Year to archive, for example 2024 or 2014..=2020"
    )]
    year: String,

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
        default_value_t = 8,
        help = "Concurrent Gmail requests for download and optional removal"
    )]
    concurrency: usize,

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

    #[arg(
        long,
        help = "Move each message to Gmail trash after it has been staged successfully"
    )]
    remove: bool,
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
    let year_selection = YearSelection::parse(&args.year)?;

    let page_size = args.page_size.clamp(1, 500);
    let concurrency = args.concurrency.max(1);
    let token_store = args.token_store.unwrap_or_else(default_token_store_path);
    let oauth_scope = if args.remove {
        auth::GMAIL_MODIFY_SCOPE
    } else {
        auth::GMAIL_READONLY_SCOPE
    };

    println!("Years: {year_selection}");
    println!("Concurrency: {concurrency}");
    if args.remove {
        println!("Remove mode: enabled (messages will be moved to Gmail trash after staging)");
    }

    let http_client = auth::build_http_client()?;
    let authenticator = auth::Authenticator::from_client_secret_file(
        http_client.clone(),
        &args.oauth_client,
        token_store,
        oauth_scope,
    )?;
    let gmail_client = gmail::GmailClient::new(http_client, authenticator, args.include_spam_trash);

    let years = year_selection.years();
    let mut overall = OverallSummary::default();

    for (index, year) in years.iter().copied().enumerate() {
        let year_window = YearWindow::for_year(year)?;
        let query = year_window.build_query(args.query.as_deref());
        let output_path = output_path_for_year(args.output.as_deref(), &year_selection, year)?;
        let work_dir = work_dir_for_year(
            args.work_dir.as_deref(),
            &year_selection,
            year,
            &query,
            args.include_spam_trash,
            args.remove,
        )?;

        if years.len() > 1 {
            println!();
            println!("=== Year {year} ({}/{}) ===", index + 1, years.len());
        }

        println!(
            "Time range: {} to {}",
            year_window.start_local.to_rfc3339(),
            year_window.end_local.to_rfc3339()
        );
        println!("Gmail query: {query}");
        println!("Work directory: {}", work_dir.display());
        println!("Output: {}", output_path.display());

        let summary = archive::write_archive(
            &gmail_client,
            archive::ArchiveRequest {
                year,
                query,
                start_local: year_window.start_local.to_rfc3339(),
                end_local: year_window.end_local.to_rfc3339(),
                output_path,
                work_dir,
                page_size,
                concurrency,
                include_spam_trash: args.include_spam_trash,
                remove_after_stage: args.remove,
            },
        )
        .await?;

        println!("Matched messages: {}", summary.message_count);
        println!("Reused staged messages: {}", summary.reused_messages);
        println!("Downloaded messages: {}", summary.downloaded_messages);
        println!("Trashed messages: {}", summary.removed_messages);
        println!(
            "Already in Gmail trash: {}",
            summary.already_trashed_messages
        );
        println!(
            "Failed Gmail trash attempts: {}",
            summary.failed_remove_messages
        );
        println!("Wrote archive: {}", summary.output_path.display());

        overall.add(&summary);
    }

    if years.len() > 1 {
        println!();
        println!("=== Overall Summary ===");
        println!("Years processed: {}", years.len());
        println!("Matched messages: {}", overall.message_count);
        println!("Reused staged messages: {}", overall.reused_messages);
        println!("Downloaded messages: {}", overall.downloaded_messages);
        println!("Trashed messages: {}", overall.removed_messages);
        println!(
            "Already in Gmail trash: {}",
            overall.already_trashed_messages
        );
        println!(
            "Failed Gmail trash attempts: {}",
            overall.failed_remove_messages
        );
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct YearSelection {
    start: i32,
    end: i32,
}

impl YearSelection {
    fn parse(raw: &str) -> Result<Self> {
        if let Some((start, end)) = raw.split_once("..=") {
            let start = parse_year_value(start.trim(), "`--year` range start")?;
            let end = parse_year_value(end.trim(), "`--year` range end")?;
            if start > end {
                bail!("`--year` range start must be less than or equal to the end");
            }
            return Ok(Self { start, end });
        }

        let year = parse_year_value(raw.trim(), "`--year`")?;
        Ok(Self {
            start: year,
            end: year,
        })
    }

    fn years(&self) -> Vec<i32> {
        (self.start..=self.end).collect()
    }

    fn is_single_year(&self) -> bool {
        self.start == self.end
    }
}

impl fmt::Display for YearSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_single_year() {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}..={}", self.start, self.end)
        }
    }
}

#[derive(Default)]
struct OverallSummary {
    message_count: usize,
    reused_messages: usize,
    downloaded_messages: usize,
    removed_messages: usize,
    already_trashed_messages: usize,
    failed_remove_messages: usize,
}

impl OverallSummary {
    fn add(&mut self, summary: &archive::ArchiveSummary) {
        self.message_count += summary.message_count;
        self.reused_messages += summary.reused_messages;
        self.downloaded_messages += summary.downloaded_messages;
        self.removed_messages += summary.removed_messages;
        self.already_trashed_messages += summary.already_trashed_messages;
        self.failed_remove_messages += summary.failed_remove_messages;
    }
}

fn default_token_store_path() -> PathBuf {
    if let Some(config_dir) = dirs::config_dir() {
        return config_dir.join("gmail-archiver").join("token.json");
    }

    PathBuf::from(".gmail-archiver").join("token.json")
}

fn default_work_dir_name(
    year: i32,
    query: &str,
    include_spam_trash: bool,
    remove_after_stage: bool,
) -> String {
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
    hasher.update(b"\n");
    hasher.update(if remove_after_stage {
        "true".as_bytes()
    } else {
        "false".as_bytes()
    });

    let digest = hasher.finalize();
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    format!("{year}-{hash}")
}

fn parse_year_value(raw: &str, label: &str) -> Result<i32> {
    let year = raw
        .parse::<i32>()
        .with_context(|| format!("{label} must be a 4-digit year"))?;
    if !(1970..=9999).contains(&year) {
        bail!("{label} must be between 1970 and 9999");
    }
    Ok(year)
}

fn output_path_for_year(
    output: Option<&Path>,
    year_selection: &YearSelection,
    year: i32,
) -> Result<PathBuf> {
    match output {
        Some(path) if year_selection.is_single_year() => Ok(path.to_path_buf()),
        Some(path) => {
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
            {
                bail!(
                    "`--output` must point to a directory when `--year` is a range like 2014..=2020"
                );
            }
            Ok(path.join(format!("gmail-{year}.zip")))
        }
        None => Ok(PathBuf::from("archives").join(format!("gmail-{year}.zip"))),
    }
}

fn work_dir_for_year(
    work_dir: Option<&Path>,
    year_selection: &YearSelection,
    year: i32,
    query: &str,
    include_spam_trash: bool,
    remove_after_stage: bool,
) -> Result<PathBuf> {
    let default_name = default_work_dir_name(year, query, include_spam_trash, remove_after_stage);

    match work_dir {
        Some(path) if year_selection.is_single_year() => Ok(path.to_path_buf()),
        Some(path) => Ok(path.join(default_name)),
        None => Ok(PathBuf::from(".gmail-archiver-work").join(default_name)),
    }
}
