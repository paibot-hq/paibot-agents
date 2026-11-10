//! 初期設定と設定の画面

use std::path::Path;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align, Id, Layout, RichText, ScrollArea, TextEdit, vec2};

use crate::config::{Agent, Config};
use crate::projects;
use crate::theme::*;
use crate::util::{expand, tilde};

struct AgentRow {
    on: bool,
    agent: Agent,
    found: Option<bool>,
}

struct Detected {
    agents: Vec<String>,
    editors: Vec<String>,
}

pub struct Setup {
    first_run: bool,
    roots: Vec<String>,
    suggestions: Vec<String>,
    root_input: String,
    rows: Vec<AgentRow>,
    default_agent: String,
    editor: String,
    editors: Vec<String>,
    notify: bool,
    font_size: f32,
    workspaces: Vec<String>,
    vscode_recent: bool,
    found_ws: Vec<(String, usize)>,
    base: Config,
    detected: Arc<Mutex<Option<Detected>>>,
    applied_detection: bool,
}

pub enum Outcome {
    Keep,
    Cancel,
    Save(Config),
}

impl Setup {
    pub fn new(cfg: &Config, first_run: bool, launch: &Path, ctx: &egui::Context) -> Self {
        let suggestions = projects::suggest_roots(launch);
        let roots = if first_run && cfg.project_roots.is_empty() {
            suggestions.iter().take(1).cloned().collect()
        } else {
            cfg.project_roots.clone()
        };
        let rows = cfg.agents.iter().map(|a| AgentRow { on: true, agent: a.clone(), found: None }).collect();
        let detected = Arc::new(Mutex::new(None));
        {
            let detected = detected.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let d = Detected { agents: projects::detect_agents(), editors: projects::detect_editors() };
                *detected.lock().unwrap() = Some(d);
                ctx.request_repaint();
            });
        }
        Self {
            first_run,
            roots,
            suggestions,
            root_input: String::new(),
            rows,
            default_agent: cfg.default_agent.clone(),
            editor: cfg.editor_app.clone(),
            editors: Vec::new(),
            notify: cfg.notify,
            workspaces: cfg.workspaces.clone(),
            vscode_recent: cfg.vscode_recent,
            found_ws: Vec::new(),
            font_size: cfg.font_size,
            base: cfg.clone(),
            detected,
            applied_detection: false,
        }
    }

    /// 見つかったエージェントを一覧に反映する（初回は見つかったものだけを有効に）
    fn apply_detection(&mut self) {
        if self.applied_detection {
            return;
        }
        let Some(d) = self.detected.lock().unwrap().take() else { return };
        self.applied_detection = true;
        for r in &mut self.rows {
            let cmd = r.agent.cmd.split_whitespace().next().unwrap_or("").to_string();
            r.found = Some(cmd.is_empty() || d.agents.contains(&cmd));
            if self.first_run {
                r.on = r.found == Some(true);
            }
        }
        for a in d.agents {
            if !self.rows.iter().any(|r| r.agent.cmd.split_whitespace().next() == Some(a.as_str())) {
                // shell の前に入れる
                let pos = self.rows.iter().position(|r| r.agent.cmd.is_empty()).unwrap_or(self.rows.len());
                self.rows.insert(pos, AgentRow { on: true, agent: Agent { name: a.clone(), cmd: a }, found: Some(true) });
            }
        }
        self.editors = d.editors;
        if self.first_run && !self.editors.contains(&self.editor) {
            if let Some(e) = self.editors.first() {
                self.editor = e.clone();
            }
        }
        if !self.rows.iter().any(|r| r.on && r.agent.name == self.default_agent) {
            if let Some(r) = self.rows.iter().find(|r| r.on) {
                self.default_agent = r.agent.name.clone();
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        self.apply_detection();
        let mut out = Outcome::Keep;
        let screen = ctx.screen_rect();
        let modal = egui::Modal::new(Id::new("setup")).show(ctx, |ui| {
            ui.set_width(640.0_f32.min(screen.width() - 80.0));
            let title = if self.first_run { "はじめに" } else { "設定" };
            ui.label(RichText::new(title).font(crate::ui::bold(19.0)).color(TEXT));
            if self.first_run {
                ui.add_space(4.0);
                ui.label(RichText::new("ここで決めたことは、あとから ⌘, でいつでも変えられます。").color(MUTED));
            }
            ui.add_space(14.0);

            ScrollArea::vertical().max_height((screen.height() - 260.0).max(240.0)).auto_shrink([false, true]).show(ui, |ui| {
                section(ui, "プロジェクトの置き場", "この中のフォルダが、新しいセッションの一覧に並びます。");
                let mut remove = None;
                for (i, r) in self.roots.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(r).font(egui::FontId::monospace(13.0)).color(TEXT));
                        let n = std::fs::read_dir(expand(r)).map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count()).unwrap_or(0);
                        ui.label(RichText::new(format!("{n} フォルダ")).size(11.0).color(FAINT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("外す").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    self.roots.remove(i);
                }
                ui.horizontal(|ui| {
                    if ui.button("フォルダを足す…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_directory(expand("~")).pick_folder() {
                            let t = tilde(&p);
                            if !self.roots.contains(&t) {
                                self.roots.push(t);
                            }
                        }
                    }
                    let r = ui.add(TextEdit::singleline(&mut self.root_input).hint_text("または場所を入力して Enter").desired_width(260.0));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && expand(&self.root_input).is_dir() {
                        let t = tilde(&expand(&self.root_input));
                        if !self.roots.contains(&t) {
                            self.roots.push(t);
                        }
                        self.root_input.clear();
                    }
                });
                let sug: Vec<String> = self.suggestions.iter().filter(|s| !self.roots.contains(s)).cloned().collect();
                if !sug.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("候補:").size(12.0).color(FAINT));
                        for s in sug {
                            if ui.small_button(format!("＋ {s}")).clicked() {
                                self.roots.push(s);
                            }
                        }
                    });
                }

                ui.add_space(18.0);
                section(ui, "エージェント", "使うものに印を付けます。◉ が新しいセッションで最初に選ばれます。");
                if !self.applied_detection {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(RichText::new("入っているものを探しています…").size(12.0).color(MUTED));
                    });
                }
                let mut remove = None;
                for (i, r) in self.rows.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut r.on, "");
                        ui.add_enabled_ui(r.on, |ui| {
                            ui.radio_value(&mut self.default_agent, r.agent.name.clone(), "");
                        });
                        ui.add(TextEdit::singleline(&mut r.agent.name).desired_width(110.0));
                        ui.add(
                            TextEdit::singleline(&mut r.agent.cmd)
                                .desired_width(220.0)
                                .font(egui::FontId::monospace(12.0))
                                .hint_text("空ならシェル"),
                        );
                        match r.found {
                            Some(true) => ui.label(RichText::new("見つかった").size(11.0).color(OK)),
                            Some(false) => ui.label(RichText::new("見つからない").size(11.0).color(FAINT)),
                            None => ui.label(""),
                        };
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("消す").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    self.rows.remove(i);
                }
                if ui.button("＋ エージェントを足す").clicked() {
                    self.rows.push(AgentRow { on: true, agent: Agent { name: "new".into(), cmd: String::new() }, found: None });
                }

                ui.add_space(18.0);
                section(ui, "VSCode", "VSCode のワークスペースや、最近開いたフォルダも新しいセッションの一覧に並べます（読むだけです）。");
                ui.checkbox(&mut self.vscode_recent, "VSCode・Cursor などで最近開いたフォルダとワークスペースを並べる");
                if self.found_ws.is_empty() {
                    let mut dirs = Vec::new();
                    for r in &self.roots {
                        let d = expand(r);
                        if let Some(p) = d.parent() {
                            dirs.push(p.to_path_buf());
                        }
                        dirs.push(d);
                    }
                    for f in crate::vscode::find_workspace_files(&dirs) {
                        let n = crate::vscode::parse_workspace(&f).map(|w| w.folders.len()).unwrap_or(0);
                        self.found_ws.push((tilde(&f), n));
                    }
                    if self.found_ws.is_empty() {
                        self.found_ws.push((String::new(), 0));
                    }
                }
                for (f, n) in self.found_ws.iter().filter(|(f, _)| !f.is_empty()) {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(f).font(egui::FontId::monospace(12.0)).color(TEXT));
                        ui.label(RichText::new(format!("{n} フォルダ · 置き場から自動で見つけた")).size(11.0).color(FAINT));
                    });
                }
                let mut remove = None;
                for (i, w) in self.workspaces.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(w).font(egui::FontId::monospace(12.0)).color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("外す").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    self.workspaces.remove(i);
                }
                if ui.button("ワークスペースを足す…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("VSCode ワークスペース", &["code-workspace"]).set_directory(expand("~")).pick_file() {
                        let t = tilde(&p);
                        if !self.workspaces.contains(&t) {
                            self.workspaces.push(t);
                        }
                    }
                }

                ui.add_space(18.0);
                section(ui, "エディタ", "「エディタで開く」に使うアプリ");
                let mut choices = self.editors.clone();
                if !choices.contains(&self.editor) {
                    choices.insert(0, self.editor.clone());
                }
                egui::ComboBox::from_id_salt("editor").selected_text(&self.editor).width(260.0).show_ui(ui, |ui| {
                    for e in choices {
                        ui.selectable_value(&mut self.editor, e.clone(), e);
                    }
                });

                ui.add_space(18.0);
                section(ui, "そのほか", "");
                ui.checkbox(&mut self.notify, "エージェントの手が空いたら通知する");
                ui.horizontal(|ui| {
                    ui.label("ターミナルの文字の大きさ");
                    ui.add(egui::Slider::new(&mut self.font_size, 10.0..=20.0).step_by(1.0));
                });
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let label = if self.first_run { "はじめる" } else { "保存" };
                let ok = egui::Button::new(RichText::new(format!("{}  {label}", crate::ui::icon::CHECK)).size(13.5).color(egui::Color32::WHITE))
                    .min_size(vec2(110.0, 32.0))
                    .corner_radius(7.0)
                    .fill(ACCENT.gamma_multiply(0.55));
                let has_agent = self.rows.iter().any(|r| r.on && !r.agent.name.trim().is_empty());
                if ui.add_enabled(has_agent, ok).clicked() {
                    out = Outcome::Save(self.build());
                }
                if ui.button(if self.first_run { "あとで" } else { "やめる" }).clicked() {
                    out = Outcome::Cancel;
                }
                if !has_agent {
                    ui.label(RichText::new("エージェントを1つ以上選んでください").size(12.0).color(WAITING));
                }
            });
        });
        if modal.should_close() && matches!(out, Outcome::Keep) {
            return Outcome::Cancel;
        }
        out
    }

    fn build(&self) -> Config {
        let mut c = self.base.clone();
        c.project_roots = self.roots.clone();
        c.agents = self
            .rows
            .iter()
            .filter(|r| r.on && !r.agent.name.trim().is_empty())
            .map(|r| Agent { name: r.agent.name.trim().to_string(), cmd: r.agent.cmd.trim().to_string() })
            .collect();
        c.default_agent = if c.agents.iter().any(|a| a.name == self.default_agent) {
            self.default_agent.clone()
        } else {
            c.agents.first().map(|a| a.name.clone()).unwrap_or_default()
        };
        c.editor_app = self.editor.clone();
        c.notify = self.notify;
        c.workspaces = self.workspaces.clone();
        c.vscode_recent = self.vscode_recent;
        c.font_size = self.font_size;
        c
    }
}

fn section(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.label(RichText::new(title).font(crate::ui::bold(13.5)).color(TEXT));
    if !sub.is_empty() {
        ui.label(RichText::new(sub).size(12.0).color(MUTED));
    }
    ui.add_space(4.0);
}
