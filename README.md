# gmail-archiver

`gmail-archiver` は、OAuth で Gmail にログインし、指定した年または年の範囲に一致するメールを取得して、`.eml` を含む zip ファイルとして保存する Rust 製 CLI です。

## 主な機能

- Google アカウントで installed-app OAuth フローを使ってサインインできる
- Gmail クエリで年ごとにメールを絞り込める
- Gmail API から `raw` メッセージを取得できる
- `messages/*.eml` と `manifest.json` を zip にまとめて保存できる
- ローカルの作業ディレクトリを使って中断から再開できる
- `--remove` でアーカイブ後に Gmail のゴミ箱へ移動できる
- Gmail ダウンロードを並列実行できる
- verify、download、trash、zip 各フェーズでターミナルのプログレスバーを表示できる
- `2014..=2020` のような年範囲を指定しても、年ごとに出力と再開状態を分けて処理できる

## 事前準備

- Rust と Cargo
- Gmail API を有効化した Google Cloud プロジェクト
- Google Cloud で作成した Desktop OAuth クライアント

## Google Cloud の設定

1. Google Cloud プロジェクトを作成します。
2. `Gmail API` を有効化します。
3. OAuth 同意画面を設定します。
4. Desktop app 用の `OAuth client ID` を作成します。
5. ダウンロードした client JSON を、このディレクトリに `client_secret.json` として置きます。

このツールは Google の installed-app OAuth フローを使います。初回実行時にはブラウザでサインインを求められ、取得したトークンは再利用できるよう保存されます。

ファイルの形を先に確認したい場合は `client_secret.example.json` を見てください。プレースホルダを実際の値に置き換えるか、Google Cloud からダウンロードした JSON をそのまま `client_secret.json` にリネームするのが簡単です。

ここでは `Web application` の OAuth クライアントは使わないでください。このツールは `127.0.0.1` のループバック redirect を使うため、web クライアントだと `redirect_uri_mismatch` になりやすいです。

## 使い方

基本実行:

```powershell
cargo run -- --year 2024
```

年範囲をまとめてアーカイブ:

```powershell
cargo run -- --year 2014..=2020
```

サンプルファイルから始める場合:

```powershell
Copy-Item .\client_secret.example.json .\client_secret.json
```

その後 `client_secret.json` を開き、Google Cloud から取得した実際の値でプレースホルダを置き換えてください。

追加の Gmail 検索条件を付ける:

```powershell
cargo run -- --year 2024 --query "label:work from:boss@example.com"
```

並列数を調整する:

```powershell
cargo run -- --year 2024 --concurrency 16
```

出力先とトークン保存先を指定する:

```powershell
cargo run -- --year 2024 `
  --output .\archives\work-2024.zip `
  --token-store .\.gmail-archiver\token.json
```

年範囲を指定した場合、`--output` はディレクトリとして扱われ、その中に `gmail-<year>.zip` が出力されます:

```powershell
cargo run -- --year 2014..=2020 `
  --output .\archives\range-2014-2020 `
  --token-store .\.gmail-archiver\token.json
```

再開用の作業ディレクトリを指定する:

```powershell
cargo run -- --year 2024 `
  --work-dir .\.gmail-archiver-work\2024-main
```

年範囲を指定した場合、`--work-dir` は親ディレクトリとして扱われ、各年ごとに再開用サブディレクトリが作られます:

```powershell
cargo run -- --year 2014..=2020 `
  --work-dir .\.gmail-archiver-work\range-2014-2020
```

spam と trash を除外する:

```powershell
cargo run -- --year 2024 --include-spam-trash false
```

ステージ完了後に Gmail のゴミ箱へ移動する:

```powershell
cargo run -- --year 2024 --remove
```

中断後に再開する:

```powershell
cargo run -- --year 2024
```

`Ctrl+C`、クラッシュ、ネットワークエラーなどで止まったあとに同じコマンドを再実行すると、既存の `.eml` を再利用しながら残りの処理を続行します。

## 出力内容

- `archives/gmail-<year>.zip`
- zip 内の `messages/*.eml`
- zip 内の `manifest.json`
- 実行中または再開用に使う `.gmail-archiver-work/<year>-<hash>/`

## 補足

- 年の絞り込みには Gmail の `after:` と `before:` 検索演算子を使います。
- `--year` は `2024` のような単年指定と、`2014..=2020` のような inclusive range 指定の両方に対応しています。
- Gmail の日付解釈を PST 基準に引っ張られないよう、クエリでは日付文字列ではなく Unix epoch 秒を使っています。
- 年範囲指定でも処理は 1 年ずつ実行し、出力ファイルと再開状態は年ごとに分離されます。
- メール一覧取得には `users.messages.list` を使います。
- メール本文の取得には `users.messages.get(format=raw)` を使います。
- ダウンロードは既定で `--concurrency 8` の並列実行です。
- 中断再開時の staged `.eml` 検証も、同じ `--concurrency` 上限で並列実行されます。
- `HTTP 429` や `5xx` のような一時的な Gmail API エラーは、自動でバックオフ再試行します。
- 通常は `https://www.googleapis.com/auth/gmail.readonly` スコープを使います。
- `--remove` を付けると `https://www.googleapis.com/auth/gmail.modify` スコープに切り替わり、完全削除ではなく Gmail のゴミ箱へ移動します。
- 再開状態は `state.json` に保存され、各メッセージは最終 zip 作成前に `messages/<message-id>.eml` としてステージされます。
- staged `.eml` を再利用するのは、`state.json` に保存された SHA-256 と一致した場合だけです。
- `--remove` 有効時は、すでにゴミ箱へ移動済みのメッセージ状態も記録し、再開時に続きから処理できます。
- Gmail の trash API を呼ぶ前に `TRASH` ラベルを確認し、すでにゴミ箱にあるメールはできるだけスキップします。
- Gmail の trash が失敗したメールはログを出していったんスキップし、`state.json` 上は未完了のまま残すので次回再試行されます。

## 注意事項

- Google Cloud 側の OAuth 設定が完了していないとサインインできません。
- OAuth クライアントがまだ testing 状態の場合、Google から未確認アプリの警告が表示されることがあります。
- メールボックスが大きいと、エクスポートに時間がかかることがあります。
