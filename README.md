# paispace

コーディングエージェント（claude / codex / シェル）を並べて動かす macOS のデスクトップアプリ。Rust 製（egui）。

- 左にセッションのカード。動いている／待っている／終了、ブランチ、最近の出力1行、変更量（+12 −3 · 4ファイル）
- 右にそのセッションのターミナル。日本語入力、コピー（ドラッグして ⌘C）、貼り付け、ホイールで履歴
- 手が空いたら macOS の通知
- ブランチ名を入れると git worktree を作ってそこで起動
- エディタ: 真ん中はタブ（「エージェント」＋開いたファイル）。色付き・行番号付きで編集して ⌘S で保存。ファイルを開いている間はエージェントを下に出しておける（⌘J）。エージェントが書き換えたら自動で読み直し、こちらに未保存の変更があるときは上書きせずに知らせる
- エージェントに聞く（⌘L）: 選んだ行（または差分のまとまりの「聞く」）について一言書くと「src/x.rs の 12〜20 行目: …」としてエージェントの入力欄に入る。⌘Enter ならそのまま送る
- ファイルパネル（⌘E）: フォルダのツリー（変更のあるファイルに色）。クリックでタブに開く
- ファイルを名前から開く（⌘P）、全体を検索（⌘⇧F、rg があれば rg）
- 差分パネル（⌘D）、エディタ／Finder で開く
- 新しいセッション（⌘N）: プロジェクトの一覧から選んで Enter ですぐ始まる。文字を打つと絞り込み、Tab でエージェントを切り替え
- VSCode を読む（書き込みはしない）: `.code-workspace` のフォルダと、VSCode・Cursor・Windsurf で最近開いたフォルダ／ワークスペースを新規作成の一覧に並べる。`paispace work.code-workspace` で直接開ける。新規作成の「ワークスペースを開く…」（⌘O）でファイルを選ぶこともでき、選んだものは設定に覚える。複数フォルダのワークスペースは「（ワークスペース）」の項目で始めると、VSCode と同じくそのフォルダだけが見える（ツリー・⌘P・検索・差分はフォルダごと、エージェントは1つ目のフォルダで起動して残りを --add-dir で渡す）
- セッションを覚える: 閉じて開き直すと、サイドバーの「前回のセッション」から再開できる（開いていたファイルのタブも戻る）。claude は `--session-id` で決めた会話を `--resume` で、codex は `~/.codex/sessions` から見つけた会話を `codex resume` で、続きから戻る。そのほかは同じフォルダで開き直す。覚えているのは `~/.config/paispace/sessions.json`
- 初めて起動したときに設定画面（あとから ⌘,）。プロジェクトの置き場、入っているエージェント（自動で探す）、エディタを選ぶ
- お節介はしない: フックを入れない、エージェントの設定に触らない、worktree を勝手に消さない

## 入れる

```sh
cargo install --path .
paispace
```

Dock から開きたいときは `sh scripts/bundle.sh ~/Applications` で `Paispace.app` を作る。

`paispace ~/work/x` のようにフォルダを渡すと、新規作成の最初のフォルダがそこになる。`paispace -a claude` で起動と同時に1つ開く。`paispace ~/work/work.code-workspace` でワークスペースのフォルダから選ぶ。

## キー

| キー | |
|---|---|
| ⌘N | 新しいセッション |
| ⌘O | VSCode のワークスペース（.code-workspace）を開く |
| ⌘1〜9 / ⌘[ ⌘] | セッションを切り替える |
| ⌘P | ファイルを名前から開く |
| ⌘⇧F | 全体を検索 |
| ⌘S | 保存 |
| ⌘L | 選んだところをエージェントに聞く |
| ⌘J | ファイルを開いている間、エージェントを下に出す／隠す |
| ⌘⌥← → | タブを移る |
| ⌘E | ファイルパネル |
| ⌘D | 差分パネル |
| ⌘, | 設定 |
| ⌘W | ファイルのタブを閉じる（エージェントのタブなら止める） |
| ⌘C / ⌘V | コピー / 貼り付け |
| ⌘+ / ⌘- / ⌘0 | 文字の大きさ |

⌘ 以外のキーはすべてそのままエージェントに届く。カードは右クリックでメニュー、ダブルクリックで名前の変更。

## 設定 `~/.config/paispace/config.toml`

ふつうは設定画面（⌘,）で変える。ファイルを直接書いてもよい。

```toml
project_roots = ["~/work"]    # この中のフォルダが新規作成の一覧に並ぶ
projects = []                 # 個別に並べたいフォルダ
workspaces = []               # 並べたい .code-workspace（置き場とその1つ上にあるものは自動）
vscode_recent = true          # VSCode などで最近開いたものも並べる
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

`PAISPACE_SHOT=out.png PAISPACE_SHOT_SECS=4 paispace -a shell` で、4秒後に自分の画面を PNG に書き出して終わる（`PAISPACE_SHOT_UI=new|diff|files|setup|ask|quick|search|resume` でその画面を開いた状態に、`PAISPACE_SHOT_OPEN=src/main.rs` でファイルを開いた状態に）。`PAISPACE_CONFIG_DIR` で設定の置き場を変えられる。
