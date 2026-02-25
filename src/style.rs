use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Style {
    pub title: &'static str,
    pub accent: &'static str,
    pub unread: &'static str,
    pub headline: &'static str,
    pub label: &'static str,
    pub body: &'static str,
    pub reset: &'static str,
}

impl Default for Style {
    fn default() -> Self {
        // DPN palette, foreground-only (no background) to preserve terminal transparency.
        Self {
            title: "\x1b[38;2;0;245;255m",   // neon cyan
            accent: "\x1b[38;2;255;45;149m", // magenta pulse
            unread: "\x1b[38;2;255;184;0m",  // dragon gold
            headline: "\x1b[38;2;0;245;255m",
            label: "\x1b[38;2;255;45;149m",
            body: "\x1b[38;2;255;184;0m",
            reset: "\x1b[0m",
        }
    }
}

pub fn style_from_newsboat_if_available() -> Style {
    let mut style = Style::default();

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = PathBuf::from(home).join(".newsboat").join("config");
    let Ok(text) = fs::read_to_string(path) else {
        return style;
    };

    // Very light mapping: if user configured listfocus/info colors, adopt fg only.
    // Keeps transparent bg by never emitting background color escape codes.
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("color ") {
            let parts: Vec<&str> = l.split_whitespace().collect();
            if parts.len() >= 3 {
                let role = parts[1];
                let fg = parts[2];
                if role == "listfocus" {
                    if let Some(code) = named_fg(fg) {
                        style.accent = code;
                    }
                } else if role == "info" {
                    if let Some(code) = named_fg(fg) {
                        style.title = code;
                    }
                } else if role == "listnormal_unread" {
                    if let Some(code) = named_fg(fg) {
                        style.unread = code;
                    }
                }
            }
        }
    }

    style
}

fn named_fg(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "red" => Some("\x1b[31m"),
        "green" => Some("\x1b[32m"),
        "yellow" => Some("\x1b[33m"),
        "blue" => Some("\x1b[34m"),
        "magenta" => Some("\x1b[35m"),
        "cyan" => Some("\x1b[36m"),
        "white" => Some("\x1b[37m"),
        "black" => Some("\x1b[30m"),
        "brightred" => Some("\x1b[91m"),
        "brightgreen" => Some("\x1b[92m"),
        "brightyellow" => Some("\x1b[93m"),
        "brightblue" => Some("\x1b[94m"),
        "brightmagenta" => Some("\x1b[95m"),
        "brightcyan" => Some("\x1b[96m"),
        "brightwhite" => Some("\x1b[97m"),
        _ => None,
    }
}
