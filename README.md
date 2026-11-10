# mure

コーディングエージェント（claude / codex / シェル）を並べて動かす macOS のデスクトップアプリ。Rust 製（egui）。

- 左にセッションのカード。動いている／待っている／終了、ブランチ、最近の出力1行、変更量（+12 −3 · 4ファイル）
- 右にそのセッションのターミナル。日本語入力、コピー（ドラッグして ⌘C）、貼り付け、ホイールで履歴
- 手が空いたら macOS の通知
- ブランチ名を入れると git worktree を作ってそこで起動
- ファイルパネル（⌘E）: フォルダのツリー（変更のあるファイルに色）、クリックで中身を色付き・行番号付きで表示、変更があればそのファイルの差分も。エージェントが書き換えたら自動で読み直す
- 差分パネル（⌘D）、エディタ／Finder で開く
- 新しいセッション（⌘N）: プロジェクトの一覧から選んで Enter ですぐ始まる。文字を打つと絞り込み、Tab でエージェントを切り替え
- 初めて起動したときに設定画面（あとから ⌘,）。プロジェクトの置き場、入っているエージェント（自動で探す）、エディタを選ぶ
- お節介はしない: フックを入れない、エージェントの設定に触らない、worktree を勝手に消さない

## 入れる

```sh
cargo install --path .
mure
```

Dock から開きたいときは `sh scripts/bundle.sh ~/Applications` で `Mure.app` を作る。

`mure ~/work/x` のようにフォルダを渡すと、新規作成の最初のフォルダがそこになる。`mure -a claude` で起動と同時に1つ開く。

## キー

| キー | |
|---|---|
| ⌘N | 新しいセッション |
| ⌘1〜9 / ⌘[ ⌘] | セッションを切り替える |
| ⌘E | ファイルパネル |
| ⌘D | 差分パネル |
| ⌘, | 設定 |
| ⌘W | 止める |
| ⌘C / ⌘V | コピー / 貼り付け |
| ⌘+ / ⌘- / ⌘0 | 文字の大きさ |

⌘ 以外のキーはすべてそのままエージェントに届く。カードは右クリックでメニュー、ダブルクリックで名前の変更。

## 設定 `~/.config/mure/config.toml`

ふつうは設定画面（⌘,）で変える。ファイルを直接書いてもよい。

```toml
project_roots = ["~/work"]    # この中のフォルダが新規作成の一覧に並ぶ
projects = []                 # 個別に並べたいフォルダ
default_agent = "claude"
notify = true                 # 手が空いたら通知
idle_secs = 2.0               # 出力がこの秒数止まったら「止まった」
min_busy_secs = 5.0           # 入力のあとこの秒数以上動いてから止まったら「待ち」
font_size = 13.0
editor_app = "Visual Studio Code"

[[agents]]
name = "claude"
cmd = "claude"

[[agents]]
name = "codex"
cmd = "codex"

[[agents]]
name = "shell"
cmd = ""                      # 空ならログインシェル
```

## 判定のしかた

フックを入れずに、出力の流れだけで見ている。最後に入力してから `min_busy_secs` 以上出力が続き、そのあと `idle_secs` 止まったら「待ち」。

## 確認用

`MURE_SHOT=out.png MURE_SHOT_SECS=4 mure -a shell` で、4秒後に自分の画面を PNG に書き出して終わる（`MURE_SHOT_UI=new|diff|files|setup` でその画面を開いた状態に、`MURE_SHOT_OPEN=src/main.rs` でファイルを開いた状態に）。`MURE_CONFIG_DIR` で設定の置き場を変えられる。
