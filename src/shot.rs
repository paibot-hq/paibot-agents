//! 確認用: MURE_SHOT=<出力先.png> を付けて起動すると、MURE_SHOT_SECS 秒後に
//! 自分の画面を PNG に書き出して終わる。MURE_SHOT_UI=new|diff でその画面を開いた状態にする。

use std::time::{Duration, Instant};

use eframe::egui;

pub struct Shot {
    path: String,
    at: Instant,
    requested: bool,
    pub ui: String,
}

impl Shot {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var("MURE_SHOT").ok()?;
        let secs = std::env::var("MURE_SHOT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(4.0);
        Some(Self {
            path,
            at: Instant::now() + Duration::from_secs_f64(secs),
            requested: false,
            ui: std::env::var("MURE_SHOT_UI").unwrap_or_default(),
        })
    }

    /// 撮り終わったら true
    pub fn update(&mut self, ctx: &egui::Context) -> bool {
        if !self.requested && Instant::now() >= self.at {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.requested = true;
        }
        let img = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = img {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Ok(f) = std::fs::File::create(&self.path) {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                if let Ok(mut wr) = enc.write_header() {
                    let _ = wr.write_image_data(&bytes);
                }
            }
            return true;
        }
        false
    }
}
