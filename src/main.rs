use anyhow::{anyhow, Context, Result};
use chrono::Local;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, terminal::disable_raw_mode, terminal::enable_raw_mode};
use postgres::{Client as PgClient, NoTls};
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader as XmlReader;
use regex::Regex;
use reqwest::blocking::Client;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::cmp::min;
use std::env;
use std::fs;
use std::io::{self, stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod style;
use style::{style_from_newsboat_if_available, Style};

const THOUGHT_POLICE_PATH: &str = "Areas/Eckenrode Muziekopname/Executive/Thought Police/";

#[derive(Debug, Clone)]
struct FeedRow {
    id: i64,
    url: String,
    title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArticleKind {
    Headline,
    Label,
    Body,
    Blank,
}

#[derive(Clone)]
struct ArticleLine {
    kind: ArticleKind,
    text: String,
}

impl ArticleLine {
    fn new(kind: ArticleKind, text: String) -> Self {
        Self { kind, text }
    }
}

#[derive(Debug, Clone)]
struct ArticleRow {
    id: i64,
    feed_id: i64,
    feed_title: String,
    url: String,
    title: String,
    published_at: String,
    published_ts: i64,
    summary: String,
    content: String,
    is_read: i64,
}

#[derive(Debug, Clone)]
struct FeedItem {
    title: String,
    url: String,
    published_at: String,
    published_ts: i64,
    summary: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct JsonFeed {
    items: Option<Vec<JsonFeedItem>>,
}

#[derive(Debug, Deserialize)]
struct JsonFeedItem {
    title: Option<String>,
    url: Option<String>,
    external_url: Option<String>,
    date_published: Option<String>,
    date_modified: Option<String>,
    summary: Option<String>,
    content_html: Option<String>,
    content_text: Option<String>,
}

struct App {
    conn: Connection,
    http: Client,
    feeds: Vec<FeedRow>,
    articles: Vec<ArticleRow>,
    feed_idx: usize,
    article_idx: usize,
    focus_feeds: bool,
    global_mode: bool,
    search: String,
    status: String,
    db_path: PathBuf,
    style: Style,
    auto_refresh_secs: u64,
    last_refresh_at: Instant,
}

impl App {
    fn truncate_chars(s: &str, max_chars: usize) -> String {
        s.chars().take(max_chars).collect()
    }

    fn new() -> Result<Self> {
        let db_path = resolve_db_path()?;
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        init_db(&conn)?;
        seed_feeds(&conn)?;

        let http = Client::builder()
            .user_agent("dpn-reader/0.1")
            .timeout(Duration::from_secs(20))
            .build()?;

        let mut app = Self {
            conn,
            http,
            feeds: Vec::new(),
            articles: Vec::new(),
            feed_idx: 0,
            article_idx: 0,
            focus_feeds: false,
            global_mode: false,
            search: String::new(),
            status: "j/k chrono, n unread, c comment, g global, r refresh, q quit".into(),
            db_path,
            style: style_from_newsboat_if_available(),
            auto_refresh_secs: env::var("DPN_READER_AUTO_REFRESH_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(900),
            last_refresh_at: Instant::now(),
        };
        app.reload()?;
        if app.articles.is_empty() && !app.feeds.is_empty() {
            let (ins, fail) = refresh_all(&app.conn, &app.http)?;
            app.reload()?;
            app.last_refresh_at = Instant::now();
            app.status = format!(
                "Initial refresh: +{} new, failures={} (feeds={}, articles={})",
                ins,
                fail,
                app.feeds.len(),
                app.articles.len()
            );
        } else {
            app.update_status_counts("Ready");
        }
        Ok(app)
    }

    fn reload(&mut self) -> Result<()> {
        self.feeds = load_feeds(&self.conn)?;
        if self.feed_idx >= self.feeds.len() {
            self.feed_idx = self.feeds.len().saturating_sub(1);
        }

        let feed_id = if self.global_mode {
            None
        } else {
            self.feeds.get(self.feed_idx).map(|f| f.id)
        };

        self.articles = load_articles(&self.conn, feed_id, self.search.trim())?;
        if self.article_idx >= self.articles.len() {
            self.article_idx = self.articles.len().saturating_sub(1);
        }
        Ok(())
    }

    fn update_status_counts(&mut self, prefix: &str) {
        self.status = format!(
            "{} (feeds={}, articles={}, mode={})",
            prefix,
            self.feeds.len(),
            self.articles.len(),
            if self.global_mode { "global" } else { "feed" }
        );
    }

    fn render(&self) -> Result<()> {
        let (w, h) = terminal::size()?;
        let left_w = min(40, (w as usize / 3).max(24)) as u16;

        let mut out = stdout();
        execute!(out, Clear(ClearType::All), MoveTo(0, 0))?;

        let mode = if self.global_mode { "global" } else { "feed" };
        writeln!(
            out,
            "{}dpn-reader [{}]{}  db: {}",
            self.style.title,
            mode,
            self.style.reset,
            self.db_path.display()
        )?;

        let body_h = h.saturating_sub(3) as usize;

        for row in 0..body_h {
            let y = row as u16 + 1;
            execute!(out, MoveTo(left_w, y))?;
            write!(out, "|")?;

            // feeds pane
            execute!(out, MoveTo(0, y))?;
            if row == 0 {
                if self.focus_feeds {
                    write!(out, "{}[Feeds]{}", self.style.accent, self.style.reset)?;
                } else {
                    write!(out, " Feeds ")?;
                }
            } else {
                let idx = row - 1;
                if let Some(f) = self.feeds.get(idx) {
                    let marker = if idx == self.feed_idx { ">" } else { " " };
                    let line = Self::truncate_chars(
                        &format!("{} {}", marker, f.title),
                        left_w.saturating_sub(1) as usize,
                    );
                    write!(out, "{}", line)?;
                }
            }

            // articles pane
            execute!(out, MoveTo(left_w + 2, y))?;
            if row == 0 {
                let header = if self.focus_feeds {
                    " Articles ".to_string()
                } else {
                    "[Articles]".to_string()
                };
                write!(out, "{}{}{}", self.style.accent, header, self.style.reset)?;
            } else {
                let idx = row - 1;
                if let Some(a) = self.articles.get(idx) {
                    let selected = idx == self.article_idx && !self.focus_feeds;
                    let marker = if a.is_read == 0 {
                        format!("{}•{}", self.style.unread, self.style.reset)
                    } else {
                        " ".to_string()
                    };
                    let sel = if selected { ">" } else { " " };
                    let date = if a.published_at.len() >= 10 {
                        &a.published_at[..10]
                    } else {
                        "----------"
                    };
                    let mut line = format!("{}{} {}  {}", sel, marker, date, a.title);
                    let maxw = w.saturating_sub(left_w + 3) as usize;
                    line = Self::truncate_chars(&line, maxw);
                    write!(out, "{}", line)?;
                }
            }
        }

        execute!(out, MoveTo(0, h.saturating_sub(2)))?;
        writeln!(out, "{}", "-".repeat(w as usize))?;
        execute!(out, MoveTo(0, h.saturating_sub(1)))?;
        let status = Self::truncate_chars(&self.status, w.saturating_sub(1) as usize);
        write!(out, "{}", status)?;

        out.flush()?;
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        loop {
            self.render()?;
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(key) = event::read()? {
                    if self.handle_key(key)? {
                        break;
                    }
                }
            } else if self.last_refresh_at.elapsed() >= Duration::from_secs(self.auto_refresh_secs) {
                let (ins, fail) = refresh_all(&self.conn, &self.http)?;
                self.reload()?;
                self.status = format!("Auto refresh: +{} new, failures={}", ins, fail);
                self.last_refresh_at = Instant::now();
            }
        }
        disable_raw_mode()?;
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('h') => self.focus_feeds = true,
            KeyCode::Char('l') => self.focus_feeds = false,
            KeyCode::Left => self.focus_feeds = true,
            KeyCode::Right => self.focus_feeds = false,
            KeyCode::Char('g') => {
                self.global_mode = !self.global_mode;
                self.article_idx = 0;
                self.reload()?;
                self.update_status_counts("View switched");
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus_feeds {
                    if self.feed_idx + 1 < self.feeds.len() {
                        self.feed_idx += 1;
                        self.article_idx = 0;
                        if !self.global_mode {
                            self.reload()?;
                        }
                    }
                } else if self.article_idx + 1 < self.articles.len() {
                    self.article_idx += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus_feeds {
                    if self.feed_idx > 0 {
                        self.feed_idx -= 1;
                        self.article_idx = 0;
                        if !self.global_mode {
                            self.reload()?;
                        }
                    }
                } else if self.article_idx > 0 {
                    self.article_idx -= 1;
                }
            }
            KeyCode::Char('n') => {
                if let Some(idx) = next_unread_index(&self.articles, self.article_idx) {
                    self.article_idx = idx;
                    self.status = "Jumped to next unread".into();
                } else {
                    self.status = "No unread in current view".into();
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.status = "Refreshing feeds...".into();
                let (ins, fail) = refresh_all(&self.conn, &self.http)?;
                self.reload()?;
                self.status = format!(
                    "Refresh done: +{} new, failures={} (feeds={}, articles={})",
                    ins,
                    fail,
                    self.feeds.len(),
                    self.articles.len()
                );
                self.last_refresh_at = Instant::now();
            }
            KeyCode::Char('/') => {
                if let Some(s) = prompt_line("Search (title/content):", &self.search)? {
                    self.search = s;
                } else {
                    self.search.clear();
                }
                self.article_idx = 0;
                self.reload()?;
                self.status = format!(
                    "Search: {} (articles={})",
                    if self.search.is_empty() { "(none)" } else { &self.search },
                    self.articles.len()
                );
            }
            KeyCode::Char('a') => {
                if let Some(url) = prompt_line("Add feed URL:", "")? {
                    add_feed(&self.conn, &url)?;
                    self.reload()?;
                    self.update_status_counts("Feed added");
                }
            }
            KeyCode::Char('c') => {
                if let Some(article) = self.articles.get(self.article_idx).cloned() {
                    if let Some(comment) = prompt_line("Comment:", "")? {
                        let tags = prompt_line("Tags (space-separated):", "reading")
                            .unwrap_or(Some("reading".into()))
                            .unwrap_or_else(|| "reading".into());
                        let tags_vec: Vec<String> = tags
                            .split_whitespace()
                            .map(|s| s.to_string())
                            .collect();
                        match save_comment_to_documents(&article, &comment, &tags_vec) {
                            Ok(id) => {
                                mark_read(&self.conn, article.id)?;
                                self.reload()?;
                                self.status = format!("Saved comment to Thought Police (doc #{})", id);
                            }
                            Err(e) => {
                                self.status = format!("Comment save failed: {}", e);
                            }
                        }
                    }
                } else {
                    self.status = "No article selected".into();
                }
            }
            KeyCode::Char('o') => {
                if let Some(article) = self.articles.get(self.article_idx) {
                    match open_url_in_brave(&article.url) {
                        Ok(()) => self.status = "Opened article in browser".into(),
                        Err(e) => self.status = format!("Open failed: {}", e),
                    }
                } else {
                    self.status = "No article selected".into();
                }
            }
            KeyCode::Enter => {
                if self.articles.get(self.article_idx).is_some() {
                    self.read_article_view()?;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn article_lines(article: &ArticleRow) -> Vec<ArticleLine> {
        let mut lines: Vec<ArticleLine> = Vec::new();
        lines.push(ArticleLine::new(ArticleKind::Headline, article.title.clone()));
        lines.push(ArticleLine::new(
            ArticleKind::Label,
            format!("Feed: {}", article.feed_title),
        ));
        lines.push(ArticleLine::new(
            ArticleKind::Label,
            format!(
                "Date: {}",
                if article.published_at.is_empty() {
                    "unknown"
                } else {
                    &article.published_at
                }
            ),
        ));
        lines.push(ArticleLine::new(ArticleKind::Label, format!("URL: {}", article.url)));
        lines.push(ArticleLine::new(ArticleKind::Blank, String::new()));
        let body = if article.content.trim().is_empty() {
            article.summary.clone()
        } else {
            article.content.clone()
        };
        let body = html_to_text(&body);
        for line in body.lines() {
            lines.push(ArticleLine::new(ArticleKind::Body, line.to_string()));
        }
        lines
    }

    fn wrap_lines_for_width(lines: &[ArticleLine], width: usize) -> Vec<ArticleLine> {
        if width == 0 {
            return vec![ArticleLine::new(ArticleKind::Blank, String::new())];
        }

        let mut out = Vec::new();
        for line in lines {
            if line.text.trim().is_empty() {
                out.push(ArticleLine::new(line.kind, String::new()));
                continue;
            }

            let mut current = String::new();
            for word in line.text.split_whitespace() {
                if current.is_empty() {
                    if word.chars().count() <= width {
                        current.push_str(word);
                    } else {
                        let mut chunk = String::new();
                        for ch in word.chars() {
                            chunk.push(ch);
                            if chunk.chars().count() >= width {
                                out.push(ArticleLine::new(line.kind, chunk.clone()));
                                chunk.clear();
                            }
                        }
                        current = chunk;
                    }
                    continue;
                }

                let candidate_len = current.chars().count() + 1 + word.chars().count();
                if candidate_len <= width {
                    current.push(' ');
                    current.push_str(word);
                } else {
                    out.push(ArticleLine::new(line.kind, current.clone()));
                    if word.chars().count() <= width {
                        current = word.to_string();
                    } else {
                        let mut chunk = String::new();
                        for ch in word.chars() {
                            chunk.push(ch);
                            if chunk.chars().count() >= width {
                                out.push(ArticleLine::new(line.kind, chunk.clone()));
                                chunk.clear();
                            }
                        }
                        current = chunk;
                    }
                }
            }

            if !current.is_empty() {
                out.push(ArticleLine::new(line.kind, current.clone()));
            }
        }

        if out.is_empty() {
            out.push(ArticleLine::new(ArticleKind::Blank, String::new()));
        }
        out
    }

    fn colorize_article_line(line: &ArticleLine, style: &Style) -> String {
        match line.kind {
            ArticleKind::Headline => format!("{}{}{}", style.headline, line.text, style.reset),
            ArticleKind::Label => {
                if let Some(idx) = line.text.find(':') {
                    let (prefix, rest) = line.text.split_at(idx + 1);
                    format!(
                        "{}{}{}{}{}",
                        style.label,
                        prefix,
                        style.reset,
                        style.body,
                        rest
                    )
                } else {
                    format!("{}{}{}", style.body, line.text, style.reset)
                }
            }
            ArticleKind::Body => format!("{}{}{}", style.body, line.text, style.reset),
            ArticleKind::Blank => String::new(),
        }
    }

    fn read_article_view(&mut self) -> Result<()> {
        if self.articles.is_empty() {
            self.status = "No article selected".into();
            return Ok(());
        }
        let start_id = self.articles[self.article_idx].id;
        let mut firehose = load_articles(&self.conn, None, self.search.trim())?;
        if firehose.is_empty() {
            self.status = "No articles in firehose view".into();
            return Ok(());
        }
        let mut firehose_idx = firehose
            .iter()
            .position(|a| a.id == start_id)
            .unwrap_or(0);
        let mut offset = 0usize;
        loop {
            let article = firehose[firehose_idx].clone();
            mark_read(&self.conn, article.id)?;
            if let Some(a) = firehose.get_mut(firehose_idx) {
                a.is_read = 1;
            }
            let lines = Self::article_lines(&article);

            let (w, h) = terminal::size()?;
            let body_h = h.saturating_sub(2) as usize;
            let wrapped_lines = Self::wrap_lines_for_width(&lines, w.saturating_sub(1) as usize);
            let mut out = stdout();
            execute!(out, Clear(ClearType::All), MoveTo(0, 0))?;
            writeln!(
                out,
                "dpn-reader article view (firehose)  [j/k next/prev article] [up/down scroll] [q back] [c comment] [i image] [o open]"
            )?;

            for i in 0..body_h {
                let idx = offset + i;
                if idx >= wrapped_lines.len() {
                    break;
                }
                execute!(out, MoveTo(0, (i + 1) as u16))?;
                let colored = Self::colorize_article_line(&wrapped_lines[idx], &self.style);
                write!(out, "{}", colored)?;
            }
            out.flush()?;

            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') => {
                        if firehose_idx + 1 < firehose.len() {
                            firehose_idx += 1;
                            offset = 0;
                        }
                    }
                    KeyCode::Char('k') => {
                        if firehose_idx > 0 {
                            firehose_idx -= 1;
                            offset = 0;
                        }
                    }
                    KeyCode::Down => {
                        let max_off = wrapped_lines.len().saturating_sub(body_h);
                        offset = min(max_off, offset + 1);
                    }
                    KeyCode::Up => {
                        offset = offset.saturating_sub(1);
                    }
                    KeyCode::Char('c') => {
                        if let Some(comment) = prompt_line("Comment:", "")? {
                            let tags = prompt_line("Tags (space-separated):", "reading")
                                .unwrap_or(Some("reading".into()))
                                .unwrap_or_else(|| "reading".into());
                            let tags_vec: Vec<String> = tags
                                .split_whitespace()
                                .map(|s| s.to_string())
                                .collect();
                            match save_comment_to_documents(&article, &comment, &tags_vec) {
                                Ok(id) => self.status = format!("Saved comment to Thought Police (doc #{})", id),
                                Err(e) => self.status = format!("Comment save failed: {}", e),
                            }
                        }
                    }
                    KeyCode::Char('i') => {
                        match self.show_article_image(&article) {
                            Ok(msg) => self.status = msg,
                            Err(e) => self.status = format!("Image failed: {}", e),
                        }
                        break;
                    }
                    KeyCode::Char('o') => {
                        match open_url_in_brave(&article.url) {
                            Ok(()) => self.status = "Opened article in browser".into(),
                            Err(e) => self.status = format!("Open failed: {}", e),
                        }
                    }
                    _ => {}
                }
            }
        }

        self.global_mode = true;
        self.reload()?;
        let selected_id = firehose[firehose_idx].id;
        if let Some(pos) = self.articles.iter().position(|a| a.id == selected_id) {
            self.article_idx = pos;
        }
        Ok(())
    }

    fn show_article_image(&self, article: &ArticleRow) -> Result<String> {
        let blob = if article.content.trim().is_empty() {
            article.summary.as_str()
        } else {
            article.content.as_str()
        };

        let img_url = if let Some(u) = extract_first_image_url(blob) {
            u
        } else {
            let page = self
                .http
                .get(&article.url)
                .send()
                .with_context(|| format!("download article page {}", article.url))?
                .text()
                .with_context(|| format!("read article page {}", article.url))?;
            extract_og_image_url(&page).ok_or_else(|| anyhow!("No image URL in article/feed content"))?
        };

        let resp = self
            .http
            .get(&img_url)
            .send()
            .with_context(|| format!("download image {}", img_url))?;
        let bytes = resp.bytes()?;
        if bytes.is_empty() {
            return Err(anyhow!("Downloaded image is empty"));
        }

        let ext = guess_image_ext(&img_url).unwrap_or("img");
        let tmp = env::temp_dir().join(format!(
            "dpn-reader-image-{}.{ext}",
            Local::now().format("%Y%m%d%H%M%S")
        ));
        fs::write(&tmp, &bytes)?;

        disable_raw_mode()?;
        println!("\nDisplaying image via kitty icat. Press Enter to return.\n");
        let status = Command::new("kitty")
            .args(["+kitten", "icat", tmp.to_string_lossy().as_ref()])
            .status();
        let mut wait = String::new();
        let _ = io::stdin().read_line(&mut wait);
        let _ = enable_raw_mode();

        let _ = fs::remove_file(&tmp);
        match status {
            Ok(s) if s.success() => Ok("Image displayed".into()),
            Ok(s) => Err(anyhow!("kitty icat exited with status {}", s)),
            Err(e) => Err(anyhow!("Failed to run kitty icat: {}", e)),
        }
    }
}

fn resolve_db_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("DPN_READER_DB") {
        return Ok(PathBuf::from(path));
    }

    let home = env::var("HOME").context("HOME env var not set")?;
    Ok(Path::new(&home)
        .join(".local")
        .join("share")
        .join("dpn-reader")
        .join("dpn_reader.db"))
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS feeds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            category TEXT NOT NULL DEFAULT '',
            active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            last_fetched TEXT
        );

        CREATE TABLE IF NOT EXISTS articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
            url TEXT NOT NULL,
            title TEXT NOT NULL,
            author TEXT,
            published_at TEXT,
            published_ts INTEGER NOT NULL DEFAULT 0,
            summary TEXT NOT NULL DEFAULT '',
            content TEXT NOT NULL DEFAULT '',
            is_read INTEGER NOT NULL DEFAULT 0,
            inserted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(feed_id, url)
        );

        CREATE INDEX IF NOT EXISTS idx_articles_feed_ts ON articles(feed_id, published_ts DESC);
        CREATE INDEX IF NOT EXISTS idx_articles_global_ts ON articles(published_ts DESC);
        ",
    )?;
    Ok(())
}

fn seed_feeds(conn: &Connection) -> Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM feeds", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(());
    }

    let opml_path = env::var("DPN_READER_OPML")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./feeds.opml"));

    let mut rows = parse_opml(&opml_path)?;
    if rows.is_empty() {
        let fallback_urls = env::var("DPN_READER_URLS")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = env::var("HOME").unwrap_or_else(|_| ".".into());
                Path::new(&home).join(".newsboat").join("urls")
            });
        rows = parse_newsboat_urls(&fallback_urls)?;
    }

    for (url, title, cat) in rows {
        conn.execute(
            "INSERT OR IGNORE INTO feeds(url, title, category) VALUES (?1, ?2, ?3)",
            params![url, title, cat],
        )?;
    }

    Ok(())
}

fn parse_opml(path: &Path) -> Result<Vec<(String, String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(path)?;
    let mut reader = XmlReader::from_str(&text);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<String> = Vec::new();
    let mut rows = Vec::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) => {
                if e.name().as_ref() == b"outline" {
                    let mut text_attr = String::new();
                    let mut title_attr = String::new();
                    let mut xml_url = String::new();
                    for a in e.attributes().flatten() {
                        let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
                        let v = a.unescape_value()?.to_string();
                        match k.as_str() {
                            "text" => text_attr = v,
                            "title" => title_attr = v,
                            "xmlUrl" | "xmlurl" => xml_url = v,
                            _ => {}
                        }
                    }
                    if xml_url.is_empty() {
                        let sec = if !text_attr.is_empty() { text_attr } else { title_attr };
                        stack.push(sec);
                    } else {
                        let title = if !text_attr.is_empty() { text_attr } else if !title_attr.is_empty() { title_attr } else { xml_url.clone() };
                        let category = stack.last().cloned().unwrap_or_default();
                        rows.push((xml_url, title, category));
                    }
                }
            }
            Ok(XmlEvent::End(e)) => {
                if e.name().as_ref() == b"outline" {
                    if !stack.is_empty() {
                        stack.pop();
                    }
                }
            }
            Ok(XmlEvent::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(anyhow!("OPML parse error: {}", e)),
        }
    }

    Ok(rows)
}

fn parse_newsboat_urls(path: &Path) -> Result<Vec<(String, String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    let mut section = String::new();
    let re_title = Regex::new(r#"\"([^\"]+)\""#)?;

    for raw in fs::read_to_string(path)?.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            section = line.trim_start_matches('#').trim().to_string();
            continue;
        }
        if !(line.starts_with("http://") || line.starts_with("https://")) {
            continue;
        }

        let url = line.split_whitespace().next().unwrap_or("").to_string();
        if url.is_empty() {
            continue;
        }
        let title = re_title
            .captures(line)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim_start_matches('~').trim().to_string())
            .unwrap_or_else(|| url.clone());

        rows.push((url, title, section.clone()));
    }

    Ok(rows)
}

fn parse_datetime_epoch(s: &str) -> i64 {
    if s.trim().is_empty() {
        return 0;
    }
    if let Ok(ts) = chrono::DateTime::parse_from_rfc2822(s) {
        return ts.timestamp();
    }
    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
        return ts.timestamp();
    }
    0
}

fn decode_html_entities(input: &str) -> String {
    let re_numeric = Regex::new(r"&#(x?[0-9A-Fa-f]+);").expect("valid regex");
    let out = re_numeric.replace_all(input, |caps: &regex::Captures<'_>| {
        let raw = &caps[1];
        let parsed = if let Some(hex) = raw.strip_prefix('x').or_else(|| raw.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            raw.parse::<u32>().ok()
        };
        parsed
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    });

    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
}

fn html_to_text(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if !trimmed.contains('<') && !trimmed.contains('&') {
        return trimmed.to_string();
    }

    let mut s = trimmed.replace("\r\n", "\n").replace('\r', "\n");

    // Preserve image URLs before stripping tags so `i` can still display them later.
    s = Regex::new(r#"(?is)<img[^>]+src=["']([^"']+)["'][^>]*>"#)
        .expect("valid regex")
        .replace_all(&s, "\n![image]($1)\n")
        .into_owned();

    let patterns = [
        (r"(?is)<script[^>]*>.*?</script>", " "),
        (r"(?is)<style[^>]*>.*?</style>", " "),
        (r"(?is)<br\s*/?>", "\n"),
        (r"(?is)</p\s*>", "\n\n"),
        (r"(?is)</(div|section|article|header|footer|h[1-6]|li|tr|blockquote)\s*>", "\n"),
        (r"(?is)<li[^>]*>", "- "),
    ];
    for (pat, repl) in patterns {
        s = Regex::new(pat)
            .expect("valid regex")
            .replace_all(&s, repl)
            .into_owned();
    }

    s = Regex::new(r"(?is)<[^>]+>")
        .expect("valid regex")
        .replace_all(&s, " ")
        .into_owned();

    s = decode_html_entities(&s);
    s = Regex::new(r"[ \t]+")
        .expect("valid regex")
        .replace_all(&s, " ")
        .into_owned();

    let mut out = String::new();
    let mut blank_run = 0usize;
    for raw in s.lines() {
        let line = raw.trim();
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

fn fetch_feed_items(http: &Client, url: &str) -> Result<Vec<FeedItem>> {
    let resp = http.get(url).send().with_context(|| format!("GET {} failed", url))?;
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let bytes = resp.bytes()?;

    if ctype.contains("json") || url.ends_with(".json") {
        let jf: JsonFeed = serde_json::from_slice(&bytes)?;
        let mut out = Vec::new();
        if let Some(items) = jf.items {
            for it in items {
                let link = it.url.or(it.external_url).unwrap_or_default();
                if link.is_empty() {
                    continue;
                }
                let pub_s = it.date_published.or(it.date_modified).unwrap_or_default();
                let summary = html_to_text(&it.summary.unwrap_or_default());
                let content_raw = it
                    .content_html
                    .or(it.content_text)
                    .unwrap_or_else(|| summary.clone());
                let content = html_to_text(&content_raw);
                out.push(FeedItem {
                    title: it.title.unwrap_or_else(|| "Untitled".into()),
                    url: link,
                    published_ts: parse_datetime_epoch(&pub_s),
                    published_at: pub_s,
                    summary,
                    content,
                });
            }
        }
        return Ok(out);
    }

    // Try RSS first
    if let Ok(channel) = rss::Channel::read_from(&bytes[..]) {
        let items = channel
            .items()
            .iter()
            .filter_map(|it| {
                let link = it.link().unwrap_or("").to_string();
                if link.is_empty() {
                    return None;
                }
                let title = it.title().unwrap_or("Untitled").to_string();
                let pub_s = it.pub_date().unwrap_or("").to_string();
                let summary = html_to_text(it.description().unwrap_or(""));
                let content = html_to_text(it.content().unwrap_or(&summary));
                Some(FeedItem {
                    title,
                    url: link,
                    published_ts: parse_datetime_epoch(&pub_s),
                    published_at: pub_s,
                    summary,
                    content,
                })
            })
            .collect();
        return Ok(items);
    }

    // Fall back to Atom
    let feed = atom_syndication::Feed::read_from(&bytes[..])
        .with_context(|| format!("parse RSS/Atom failed for {}", url))?;
    let mut out = Vec::new();
    for e in feed.entries() {
        let link = e
            .links()
            .iter()
            .find(|l| l.rel() == "alternate")
            .or_else(|| e.links().first())
            .map(|l| l.href().to_string())
            .unwrap_or_default();
        if link.is_empty() {
            continue;
        }
        let title = e.title().to_string();
        let pub_s = e.updated().to_rfc3339();
        let summary = html_to_text(
            &e.summary()
                .map(|s| s.to_string())
                .unwrap_or_default(),
        );
        let content_raw = e
            .content()
            .and_then(|c| c.value().map(|v| v.to_string()))
            .unwrap_or_else(|| summary.clone());
        let content = html_to_text(&content_raw);
        out.push(FeedItem {
            title,
            url: link,
            published_ts: parse_datetime_epoch(&pub_s),
            published_at: pub_s,
            summary,
            content,
        });
    }
    Ok(out)
}

fn extract_og_image_url(page_html: &str) -> Option<String> {
    let patterns = [
        r#"(?is)<meta[^>]+property=["']og:image["'][^>]+content=["']([^"']+)["'][^>]*>"#,
        r#"(?is)<meta[^>]+content=["']([^"']+)["'][^>]+property=["']og:image["'][^>]*>"#,
        r#"(?is)<meta[^>]+name=["']twitter:image["'][^>]+content=["']([^"']+)["'][^>]*>"#,
        r#"(?is)<meta[^>]+content=["']([^"']+)["'][^>]+name=["']twitter:image["'][^>]*>"#,
    ];

    for pat in patterns {
        let re = Regex::new(pat).ok()?;
        if let Some(cap) = re.captures(page_html) {
            if let Some(m) = cap.get(1) {
                let u = m.as_str().trim().to_string();
                if u.starts_with("http://") || u.starts_with("https://") {
                    return Some(u);
                }
            }
        }
    }
    None
}

fn refresh_all(conn: &Connection, http: &Client) -> Result<(usize, usize)> {
    let mut stmt = conn.prepare("SELECT id, url FROM feeds WHERE active = 1 ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut inserted_total = 0usize;
    let mut fail_total = 0usize;

    for (feed_id, url) in rows {
        match fetch_feed_items(http, &url) {
            Ok(items) => {
                let tx = conn.unchecked_transaction()?;
                for it in items {
                    tx.execute(
                        "
                        INSERT OR IGNORE INTO articles(
                          feed_id, url, title, author, published_at, published_ts, summary, content
                        ) VALUES (?1, ?2, ?3, '', ?4, ?5, ?6, ?7)
                        ",
                        params![
                            feed_id,
                            it.url,
                            it.title,
                            it.published_at,
                            it.published_ts,
                            it.summary,
                            it.content
                        ],
                    )?;
                    inserted_total += tx.changes() as usize;
                }
                tx.execute(
                    "UPDATE feeds SET last_fetched = CURRENT_TIMESTAMP WHERE id = ?1",
                    params![feed_id],
                )?;
                tx.commit()?;
            }
            Err(_) => {
                fail_total += 1;
            }
        }
    }

    Ok((inserted_total, fail_total))
}

fn load_feeds(conn: &Connection) -> Result<Vec<FeedRow>> {
    let mut stmt = conn.prepare("SELECT id, url, title FROM feeds WHERE active = 1 ORDER BY title COLLATE NOCASE")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(FeedRow {
                id: r.get(0)?,
                url: r.get(1)?,
                title: r.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_articles(conn: &Connection, feed_id: Option<i64>, search: &str) -> Result<Vec<ArticleRow>> {
    let mut sql = String::from(
        "
        SELECT a.id, a.feed_id, f.title, a.url, a.title,
               COALESCE(a.published_at,''), a.published_ts,
               COALESCE(a.summary,''), COALESCE(a.content,''), a.is_read
        FROM articles a
        JOIN feeds f ON f.id = a.feed_id
        WHERE 1=1
        ",
    );

    let mut bind_vals: Vec<String> = Vec::new();
    if let Some(fid) = feed_id {
        sql.push_str(" AND a.feed_id = ?");
        bind_vals.push(fid.to_string());
    }
    if !search.is_empty() {
        sql.push_str(" AND (a.title LIKE ? OR a.summary LIKE ? OR a.content LIKE ?)");
        let q = format!("%{}%", search);
        bind_vals.push(q.clone());
        bind_vals.push(q.clone());
        bind_vals.push(q);
    }
    sql.push_str(" ORDER BY a.published_ts DESC, a.id DESC LIMIT 500");

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = Vec::new();

    let params_iter = bind_vals.iter().map(|s| s as &dyn rusqlite::ToSql);
    let mut q = stmt.query(rusqlite::params_from_iter(params_iter))?;
    while let Some(r) = q.next()? {
        rows.push(ArticleRow {
            id: r.get(0)?,
            feed_id: r.get(1)?,
            feed_title: r.get(2)?,
            url: r.get(3)?,
            title: r.get(4)?,
            published_at: r.get(5)?,
            published_ts: r.get(6)?,
            summary: r.get(7)?,
            content: r.get(8)?,
            is_read: r.get(9)?,
        });
    }

    Ok(rows)
}

fn add_feed(conn: &Connection, url: &str) -> Result<()> {
    let title = url.trim();
    conn.execute(
        "INSERT OR IGNORE INTO feeds(url, title, category) VALUES (?1, ?2, '')",
        params![url.trim(), title],
    )?;
    Ok(())
}

fn mark_read(conn: &Connection, article_id: i64) -> Result<()> {
    conn.execute("UPDATE articles SET is_read = 1 WHERE id = ?1", params![article_id])?;
    Ok(())
}

fn next_unread_index(items: &[ArticleRow], start: usize) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    for off in 1..=items.len() {
        let i = (start + off) % items.len();
        if items[i].is_read == 0 {
            return Some(i);
        }
    }
    None
}

fn slugify(s: &str) -> String {
    let re = Regex::new(r"[^a-zA-Z0-9]+$").ok();
    let cleaned = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let collapsed = cleaned
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if let Some(re) = re {
        let _ = re;
    }
    if collapsed.is_empty() {
        "untitled".into()
    } else {
        collapsed
    }
}

fn save_comment_to_documents(article: &ArticleRow, comment: &str, tags: &[String]) -> Result<i64> {
    let now = Local::now();
    let ts = now.format("%Y-%m-%d-%H%M%S").to_string();
    let short = slugify(&article.title);
    let short = if short.len() > 60 { &short[..60] } else { &short };
    let path = format!("{}{}-{}.md", THOUGHT_POLICE_PATH, ts, short);

    let frontmatter = serde_json::json!({
        "title": format!("Reading: {}", article.title),
        "status": "draft",
        "type": "reading_comment",
        "source_url": article.url,
        "source_feed": article.feed_title,
        "source_published_at": article.published_at,
        "tags": tags,
        "created_at": now.to_rfc3339(),
    });

    let body = format!(
        "# Reading Comment\n\n## Source\n- Title: {}\n- Feed: {}\n- URL: {}\n- Published: {}\n\n## Comment\n\n{}\n",
        article.title,
        article.feed_title,
        article.url,
        if article.published_at.is_empty() { "unknown" } else { &article.published_at },
        comment.trim()
    );

    // Use dpn-api instead of direct Postgres connection
    let api_url = env::var("DPN_API_URL").unwrap_or_else(|_| "https://api.n8k99.com".into());
    let api_key = env::var("DPN_API_KEY").unwrap_or_default();
    
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(format!("{}/documents", api_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "path": path,
            "title": format!("Reading: {}", article.title),
            "frontmatter": frontmatter.to_string(),
            "content": body,
        }))
        .send()
        .with_context(|| "Could not connect to dpn-api for comment save")?;

    if !response.status().is_success() {
        anyhow::bail!("API error: {} - {}", response.status(), response.text().unwrap_or_default());
    }

    let result: serde_json::Value = response.json()
        .with_context(|| "Failed to parse API response")?;
    
    let id = result["id"].as_i64().unwrap_or(0);
    Ok(id)
}

fn extract_first_image_url(content: &str) -> Option<String> {
    // HTML <img src=\"...\">
    let re_img = Regex::new(r#"<img[^>]+src=[\"']([^\"']+)[\"']"#).ok()?;
    if let Some(cap) = re_img.captures(content) {
        if let Some(m) = cap.get(1) {
            let u = m.as_str().trim().to_string();
            if u.starts_with("http://") || u.starts_with("https://") {
                return Some(u);
            }
        }
    }

    // Markdown ![](...)
    let re_md = Regex::new(r#"!\[[^\]]*\]\((https?://[^)\s]+)\)"#).ok()?;
    if let Some(cap) = re_md.captures(content) {
        if let Some(m) = cap.get(1) {
            return Some(m.as_str().trim().to_string());
        }
    }

    None
}

fn guess_image_ext(url: &str) -> Option<&'static str> {
    let u = url.to_ascii_lowercase();
    if u.contains(".png") {
        Some("png")
    } else if u.contains(".jpg") || u.contains(".jpeg") {
        Some("jpg")
    } else if u.contains(".webp") {
        Some("webp")
    } else if u.contains(".gif") {
        Some("gif")
    } else {
        None
    }
}

fn open_url_in_brave(url: &str) -> Result<()> {
    let candidates = ["brave", "brave-browser", "brave-beta", "xdg-open"];
    for bin in candidates {
        if Command::new(bin).arg(url).spawn().is_ok() {
            return Ok(());
        }
    }
    Err(anyhow!(
        "could not launch browser; tried brave, brave-browser, brave-beta, xdg-open"
    ))
}

fn wrap_prompt_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for raw in s.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for ch in raw.chars() {
            line.push(ch);
            if line.chars().count() >= width {
                out.push(line.clone());
                line.clear();
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn prompt_line(label: &str, initial: &str) -> Result<Option<String>> {
    let mut buf = initial.to_string();
    let mut out = stdout();
    let mut last_drawn_rows: usize = 0;

    loop {
        let (w, h) = terminal::size()?;
        let width = w.saturating_sub(1) as usize;
        let wrapped = wrap_prompt_text(&buf, width);
        let mut rows = wrapped.len().max(1);
        // One extra row for the label.
        rows += 1;
        let start_row = h.saturating_sub(rows as u16);

        // Clear previously drawn prompt block.
        for i in 0..last_drawn_rows.max(rows) {
            execute!(out, MoveTo(0, h.saturating_sub(1 + i as u16)), Clear(ClearType::CurrentLine))?;
        }

        execute!(out, MoveTo(0, start_row))?;
        let mut label_line = label.to_string();
        if !initial.is_empty() {
            label_line.push_str(" (Enter=save, Shift+Enter=newline)");
        } else {
            label_line.push_str(" (Enter=save, Shift+Enter=newline)");
        }
        write!(out, "{}", App::truncate_chars(&label_line, width))?;
        for (i, line) in wrapped.iter().enumerate() {
            execute!(out, MoveTo(0, start_row + 1 + i as u16))?;
            write!(out, "{}", App::truncate_chars(line, width))?;
        }
        last_drawn_rows = rows;
        out.flush()?;

        if let Event::Key(k) = event::read()? {
            match k.code {
                KeyCode::Enter => {
                    if k.modifiers.contains(KeyModifiers::SHIFT) {
                        buf.push('\n');
                        continue;
                    }
                    for i in 0..last_drawn_rows {
                        execute!(out, MoveTo(0, h.saturating_sub(1 + i as u16)), Clear(ClearType::CurrentLine))?;
                    }
                    out.flush()?;
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        if initial.is_empty() {
                            return Ok(None);
                        }
                        return Ok(Some(initial.to_string()));
                    }
                    return Ok(Some(trimmed.to_string()));
                }
                KeyCode::Esc => {
                    for i in 0..last_drawn_rows {
                        execute!(out, MoveTo(0, h.saturating_sub(1 + i as u16)), Clear(ClearType::CurrentLine))?;
                    }
                    out.flush()?;
                    return Ok(None);
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    buf.pop();
                }
                KeyCode::Char(c) => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) && c == 'h' {
                        buf.pop();
                    } else if c == '\u{8}' || c == '\u{7f}' {
                        buf.pop();
                    } else if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                        buf.push(c);
                    }
                }
                _ => {}
            }
        }
    }
}

fn main() -> Result<()> {
    let mut app = App::new()?;
    let run_res = app.run();
    let _ = disable_raw_mode();
    run_res
}
