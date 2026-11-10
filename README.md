# mure

コーディングエージェント（claude / codex / シェル）を並べて動かすターミナルアプリ。

- 左に一覧、右に選んだセッションの画面
- 動いている（⠹）／止まってこちらを待っている（●）／終了（✓✕）が一目で分かる
- 手が空いたら macOS の通知
- ブランチ名を入れると git worktree を作ってそこで起動
- `d` でコミットしていない差分を見る
- エージェントの設定やフックには一切触らない。worktree も勝手に消さない

## 入れる

```sh
cargo install --path .
mure            # 今いるフォルダで
mure ~/work/x   # フォルダを指定
```

## キー

一覧で: `n` 新規 / `Enter`・`1〜9` 入る / `d` 差分 / `r` 名前 / `x` 止める / `PgUp` 履歴 / `q` 終わる / `?` ヘルプ

入力中は全部のキーがそのままエージェントに届く。`Ctrl-]` だけが一覧に戻るキー。

## 設定 `~/.config/mure/config.toml`

```toml
prefix = "ctrl-]"     # 一覧に戻るキー
notify = true         # 手が空いたら通知
idle_secs = 2.0       # 出力がこの秒数止まったら「止まった」
min_busy_secs = 5.0   # 入力のあとこの秒数以上動いてから止まったら「待ち」
scrollback = 5000
sidebar_width = 30

[[agents]]
name = "claude"
cmd = "claude"

[[agents]]
name = "codex"
cmd = "codex"

[[agents]]
name = "shell"
cmd = ""              # 空ならログインシェル
```

## 判定のしかた

フックを入れずに、出力の流れだけで見ている。最後に入力してから `min_busy_secs` 以上出力が続き、そのあと `idle_secs` 止まったら「待ち」。
