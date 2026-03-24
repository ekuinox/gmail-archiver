# gmail-archiver

`gmail-archiver` is a Rust CLI that signs in to Gmail with OAuth, fetches messages from a specific year or year range, and writes them into zip files as `.eml` files.

## Features

- Signs in with a Google account using the installed-app OAuth flow
- Filters messages by year with a Gmail query
- Downloads `raw` messages from the Gmail API
- Writes `messages/*.eml` plus `manifest.json` into a zip archive
- Can resume an interrupted export from a local work directory
- Can move archived messages to Gmail trash with `--remove`
- Runs Gmail downloads in parallel
- Shows terminal progress bars for verify, download, trash, and zip phases
- Can run a year range like `2014..=2020` while keeping work and output separate per year

## Prerequisites

- Rust and Cargo
- A Google Cloud project with the Gmail API enabled
- A Desktop OAuth client created in Google Cloud

## Google Cloud setup

1. Create a Google Cloud project.
2. Enable the `Gmail API`.
3. Configure the OAuth consent screen.
4. Create an `OAuth client ID` for a Desktop app.
5. Download the client JSON into this directory as `client_secret.json`.

The tool uses Google's installed-app OAuth flow. On the first run it opens a browser for sign-in, then stores the token for reuse.

If you want to see the expected file shape first, check `client_secret.example.json`. Replace every placeholder value with the real downloaded values, or more simply rename the downloaded file to `client_secret.json`.

Do not use a `Web application` OAuth client here. This tool uses a loopback redirect on `127.0.0.1`, so a web client often fails with `redirect_uri_mismatch`.

## Usage

Basic run:

```powershell
cargo run -- --year 2024
```

Archive a year range:

```powershell
cargo run -- --year 2014..=2020
```

If you are starting from the sample file:

```powershell
Copy-Item .\client_secret.example.json .\client_secret.json
```

Then open `client_secret.json` and replace the placeholder values with the real values from Google Cloud.

Add extra Gmail search terms:

```powershell
cargo run -- --year 2024 --query "label:work from:boss@example.com"
```

Tune the request concurrency:

```powershell
cargo run -- --year 2024 --concurrency 16
```

Customize output and token locations:

```powershell
cargo run -- --year 2024 `
  --output .\archives\work-2024.zip `
  --token-store .\.gmail-archiver\token.json
```

For a year range, `--output` is treated as a directory and the tool writes `gmail-<year>.zip` into it:

```powershell
cargo run -- --year 2014..=2020 `
  --output .\archives\range-2014-2020 `
  --token-store .\.gmail-archiver\token.json
```

Customize the resumable work directory:

```powershell
cargo run -- --year 2024 `
  --work-dir .\.gmail-archiver-work\2024-main
```

For a year range, `--work-dir` is treated as a parent directory and each year gets its own resumable subdirectory:

```powershell
cargo run -- --year 2014..=2020 `
  --work-dir .\.gmail-archiver-work\range-2014-2020
```

Exclude spam and trash:

```powershell
cargo run -- --year 2024 --include-spam-trash false
```

Move messages to Gmail trash after they have been staged:

```powershell
cargo run -- --year 2024 --remove
```

Resume after interruption:

```powershell
cargo run -- --year 2024
```

Run the same command again after `Ctrl+C`, a crash, or a network error. The tool keeps staged `.eml` files and continues from the remaining messages.

## Output

- `archives/gmail-<year>.zip`
- `messages/*.eml` inside the zip
- `manifest.json` inside the zip
- `.gmail-archiver-work/<year>-<hash>/` while the export is in progress or available for resume

## Notes

- Year filtering uses Gmail `after:` and `before:` search operators.
- `--year` accepts either a single year like `2024` or an inclusive range like `2014..=2020`.
- The query uses Unix epoch seconds instead of date strings to avoid Gmail's PST date interpretation.
- A year range still runs one year at a time, with a separate archive and resume state for each year.
- Message listing uses `users.messages.list`.
- Message download uses `users.messages.get(format=raw)`.
- Downloads run in parallel, with `--concurrency 8` by default.
- Resume verification of staged `.eml` files also runs in parallel, using the same `--concurrency` limit.
- Transient Gmail API failures such as HTTP 429 and 5xx are retried automatically with backoff.
- The tool requests the `https://www.googleapis.com/auth/gmail.readonly` scope.
- `--remove` upgrades the OAuth scope to `https://www.googleapis.com/auth/gmail.modify` and moves messages to Gmail trash, not permanent deletion.
- Resume state is stored in `state.json`, and each message is staged as `messages/<message-id>.eml` before the final zip is built.
- Resuming reuses a staged `.eml` only when its SHA-256 matches the hash saved in `state.json`.
- When `--remove` is enabled, the tool remembers which staged messages have already been moved to trash and continues that work after resume.
- Before calling the Gmail trash API, the tool checks whether a message already has the `TRASH` label and skips it when possible.
- If a Gmail trash request fails, the tool logs the error, skips that message for now, and continues the archive. The message is retried on the next run because it is not marked as removed in `state.json`.

## Caveats

- OAuth setup must be completed in Google Cloud before the tool can sign in.
- Google may show an unverified-app warning while the OAuth client is still in testing.
- Large mailboxes can take a while to export.
