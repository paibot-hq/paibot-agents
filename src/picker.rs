//! 新しいセッションを始める画面。プロジェクトを選んで Enter ですぐ始まる

use std::path::PathBuf;

use eframe::egui::{self, Align, Frame, Id, Key, Label, Layout, Margin, Modifiers, RichText, ScrollArea, Sense, Stroke, TextEdit, vec2};

use crate::config::Config;
use crate::projects::{self, Project};
use crate::theme::*;
use crate::util::{expand, tilde};

pub struct Picker {
    query: String,
    pub agent: usize,
    hi: usize,
    worktree: bool,
    branch: String,
    projects: Vec<Project>,
    first: bool,
    scroll_to_hi: bool,
    /// 開いたワークスペース（設定に覚えてもらう）
    pub opened_workspace: Option<PathBuf>,
    pub message: Option<String>,
}

pub enum Outcome {
    Keep,
    Cancel,
    /// roots が空でなければワークスペース（そのフォルダだけを見せる）
    Start { dir: String, agent: usize, branch: String, roots: Vec<PathBuf>, name: Option<String> },
}

impl Picker {
    pub fn new(cfg: &Config, recent: &[String]) -> Self {
        Self {
            query: String::new(),
            agent: cfg.default_agent_index(),
            hi: 0,
            worktree: false,
            branch: String::new(),
            projects: projects::list(cfg, recent),
            first: true,
            scroll_to_hi: false,
            opened_workspace: None,
            message: None,
        }
    }

    /// .code-workspace を選んで、そのフォルダを一覧の先頭に出す
    pub fn open_workspace_dialog(&mut self) {
        let Some(f) = rfd::FileDialog::new()
            .add_filter("VSCode ワークスペース", &["code-workspace"])
            .set_directory(expand("~"))
            .pick_file()
        else {
            return;
        };
        self.open_workspace(f);
    }

    pub fn open_workspace(&mut self, f: PathBuf) {
        let list = projects::workspace_projects(&f);
        if list.is_empty() {
            self.message = Some(format!("{} にフォルダが見つかりませんでした", tilde(&f)));
            return;
        }
        let paths: Vec<String> = list.iter().map(|p| p.path.clone()).collect();
        self.projects.retain(|p| !paths.contains(&p.path));
        for (i, p) in list.into_iter().enumerate() {
            self.projects.insert(i, p);
        }
        self.query.clear();
        self.hi = 0;
        self.scroll_to_hi = true;
        self.message = None;
        self.opened_workspace = Some(f);
    }

    fn filtered(&self) -> Vec<Project> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return self.projects.clone();
        }
        // 名前で一致するものを先に、場所だけで一致するものを後に
        let mut by_name: Vec<Project> = Vec::new();
        let mut by_path: Vec<Project> = Vec::new();
        for p in &self.projects {
            if p.name.to_lowercase().contains(&q) {
                by_name.push(p.clone());
            } else if p.path.to_lowercase().contains(&q) {
                by_path.push(p.clone());
            }
        }
        by_name.extend(by_path);
        by_name
    }

    pub fn show(&mut self, ctx: &egui::Context, cfg: &Config) -> Outcome {
        let n_agents = cfg.agents.len().max(1);
        // 文字入力欄に取られる前にキーを拾う
        let (down, up, tab, back_tab) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::SHIFT, Key::Tab),
            )
        });
        let enter = ctx.input(|i| i.key_pressed(Key::Enter));
        if tab {
            self.agent = (self.agent + 1) % n_agents;
        }
        if back_tab {
            self.agent = (self.agent + n_agents - 1) % n_agents;
        }
        let count = self.filtered().len();
        if down && count > 0 {
            self.hi = (self.hi + 1).min(count - 1);
            self.scroll_to_hi = true;
        }
        if up {
            self.hi = self.hi.saturating_sub(1);
            self.scroll_to_hi = true;
        }

        let mut out = Outcome::Keep;
        let mut start_dir: Option<Project> = None;
        let mut open_ws = ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O));
        let screen = ctx.screen_rect();
        let modal = egui::Modal::new(Id::new("picker")).show(ctx, |ui| {
            ui.set_width(680.0_f32.min(screen.width() - 80.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new("新しいセッション").font(crate::ui::bold(17.0)).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("↑↓ 選ぶ · Tab エージェント · Enter 始める · Esc やめる").size(11.0).color(FAINT));
                });
            });
            ui.add_space(12.0);

            // エージェント
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for (i, a) in cfg.agents.iter().enumerate() {
                    let on = i == self.agent;
                    let text = RichText::new(&a.name).size(14.0).color(if on { TEXT } else { MUTED });
                    let b = egui::Button::new(text)
                        .min_size(vec2(96.0, 34.0))
                        .corner_radius(8.0)
                        .fill(if on { ACCENT.gamma_multiply(0.30) } else { CARD })
                        .stroke(Stroke::new(1.0, if on { ACCENT } else { BORDER }));
                    if ui.add(b).clicked() {
                        self.agent = i;
                    }
                }
            });
            ui.add_space(12.0);

            // 絞り込み
            let r = ui.add(
                TextEdit::singleline(&mut self.query)
                    .hint_text(format!("{}  プロジェクトを絞り込む", crate::ui::icon::MAGNIFYING_GLASS))
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::proportional(15.0))
                    .margin(Margin::symmetric(10, 8)),
            );
            if self.first {
                r.request_focus();
                self.first = false;
            }
            if r.changed() {
                self.hi = 0;
            }
            if let Some(m) = &self.message {
                ui.label(RichText::new(m).size(12.0).color(WAITING));
            }
            ui.add_space(8.0);

            // 一覧
            let list = self.filtered();
            let hi = self.hi.min(list.len().saturating_sub(1));
            let height = (screen.height() * 0.45).clamp(160.0, 420.0);
            ScrollArea::vertical().max_height(height).min_scrolled_height(height).auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().interact_size.y = 20.0;
                ui.spacing_mut().item_spacing.y = 2.0;
                if list.is_empty() {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        let msg = if self.projects.is_empty() {
                            "プロジェクトがまだありません。下の「ワークスペースを開く…」「フォルダを選ぶ…」か、設定（⌘,）で置き場を足してください"
                        } else {
                            "見つかりません"
                        };
                        ui.label(RichText::new(msg).color(MUTED));
                    });
                }
                let mut last_group = String::new();
                for (i, p) in list.iter().enumerate() {
                    if p.group != last_group {
                        if i > 0 {
                            ui.add_space(4.0);
                        }
                        ui.label(RichText::new(&p.group).size(11.0).color(FAINT));
                        last_group = p.group.clone();
                    }
                    let on = i == hi;
                    let id = Id::new(("proj", &p.path));
                    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                    let fill = if on { CARD_SEL } else if hovered { CARD_HOVER } else { egui::Color32::TRANSPARENT };
                    let inner = Frame::new().fill(fill).corner_radius(7).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let g = if !p.roots.is_empty() {
                                crate::ui::icon::STACK
                            } else if p.group.starts_with("VSCode") {
                                crate::ui::icon::CODE
                            } else {
                                crate::ui::icon::FOLDER_SIMPLE
                            };
                            ui.label(RichText::new(g).size(14.0).color(if on { ACCENT } else { FAINT }));
                            ui.label(RichText::new(&p.name).size(13.5).color(TEXT));
                            if p.git {
                                ui.label(RichText::new(crate::ui::icon::GIT_BRANCH).size(11.0).color(FAINT));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add(Label::new(RichText::new(&p.path).size(11.0).color(FAINT)).truncate());
                            });
                        });
                    });
                    let resp = ui.interact(inner.response.rect, id, Sense::click());
                    if on && self.scroll_to_hi {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked() {
                        self.hi = i;
                    }
                    if resp.double_clicked() {
                        start_dir = Some(p.clone());
                    }
                }
            });
            self.scroll_to_hi = false;
            let chosen = list.get(hi).cloned();

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            let git = chosen.as_ref().is_some_and(|c| c.git);
            ui.horizontal(|ui| {
                ui.add_enabled(git, egui::Checkbox::new(&mut self.worktree, "新しいブランチ（worktree）で始める"));
                if self.worktree && git {
                    ui.add(
                        TextEdit::singleline(&mut self.branch)
                            .hint_text("ブランチ名")
                            .desired_width(200.0)
                            .font(egui::FontId::monospace(13.0)),
                    );
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can = chosen.is_some();
                let start = egui::Button::new(RichText::new(format!("{}  始める", crate::ui::icon::PLAY)).size(13.5).color(egui::Color32::WHITE))
                    .min_size(vec2(110.0, 32.0))
                    .corner_radius(7.0)
                    .fill(ACCENT.gamma_multiply(0.55));
                if ui.add_enabled(can, start).clicked() {
                    start_dir = chosen.clone();
                }
                if ui.button(format!("{}  ワークスペースを開く…", crate::ui::icon::CODE)).on_hover_text(".code-workspace を選ぶ  ⌘O").clicked() {
                    open_ws = true;
                }
                if ui.button("フォルダを選ぶ…").clicked() {
                    let base = chosen.as_ref().map(|c| expand(&c.path)).unwrap_or_else(|| expand("~"));
                    if let Some(p) = rfd::FileDialog::new().set_directory(base).pick_folder() {
                        start_dir = projects::folder(&p);
                    }
                }
                if ui.button("やめる").clicked() {
                    out = Outcome::Cancel;
                }
            });
            if enter && start_dir.is_none() {
                start_dir = chosen;
            }
        });
        if open_ws {
            self.open_workspace_dialog();
            return Outcome::Keep;
        }
        if modal.should_close() {
            return Outcome::Cancel;
        }
        if let Some(p) = start_dir {
            if !p.roots.is_empty() {
                let name = p.name.trim_end_matches("（ワークスペース）").to_string();
                return Outcome::Start { dir: p.path, agent: self.agent, branch: String::new(), roots: p.roots, name: Some(name) };
            }
            if self.worktree && self.branch.trim().is_empty() {
                return Outcome::Keep;
            }
            let branch = if self.worktree { self.branch.trim().to_string() } else { String::new() };
            return Outcome::Start { dir: p.path, agent: self.agent, branch, roots: Vec::new(), name: None };
        }
        out
    }
}
