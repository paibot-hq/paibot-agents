mod app;
mod config;
mod git;
mod keys;
mod notify;
mod render;
mod session;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use app::App;

const HELP: &str = "\
mure — コーディングエージェントを並べて動かすターミナル

使い方: mure [フォルダ]

  フォルダ   新しいセッションの最初のフォルダ（省略すると今いる場所）

起動したら ? でキー一覧。設定は ~/.config/mure/config.toml";

fn main() -> Result<()> {
    let mut dir = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("mure {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ if dir.is_some() => {
                anyhow::bail!("フォルダは1つだけ指定できます（mure --help）");
            }
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let dir = match dir {
        Some(d) => std::fs::canonicalize(&d)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| anyhow::anyhow!("フォルダが見つかりません: {}", d.display()))?,
        None => std::env::current_dir()?,
    };
    let (cfg, cfg_err) = config::load();

    let mut terminal = ratatui::init();
    let kitty = supports_keyboard_enhancement().unwrap_or(false);
    let _ = execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange);
    if kitty {
        // Shift+Enter などを見分けられるようにする
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let res = run(&mut terminal, cfg, cfg_err, dir);

    if kitty {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
    res
}

fn run(terminal: &mut ratatui::DefaultTerminal, cfg: config::Config, cfg_err: Option<String>, dir: PathBuf) -> Result<()> {
    let size = terminal.size()?;
    let mut app = App::new(cfg, dir, (size.width, size.height));
    if let Some(e) = cfg_err {
        app.flash(e);
    }
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    while !app.quit {
        app.tick();
        // 出力が来たらすぐ、何もなくても印のアニメーションのために時々描く
        if app.take_dirty() || last_draw.elapsed() >= Duration::from_millis(100) {
            terminal.draw(|f| render::draw(f, &mut app))?;
            last_draw = Instant::now();
        }
        if event::poll(Duration::from_millis(16))? {
            loop {
                app.handle(event::read()?);
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    app.shutdown();
    Ok(())
}
