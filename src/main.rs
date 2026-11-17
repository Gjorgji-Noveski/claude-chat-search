//! csearch: offline fuzzy search over your Claude Code conversation transcripts.
//!
//! Reads ~/.claude/projects/<encoded-folder>/<session>.jsonl, indexes each
//! top-level conversation, and lets you search them with a fuzzy score.
//! Enter resumes the selected conversation with `claude --resume <id>` in the
//! folder it came from. No network access.
//!
//! Usage:
//!   csearch                    interactive UI
//!   csearch some text          interactive UI with the query pre-filled
//!   csearch --print some text  non-interactive, prints the top results

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    style::{self, Attribute, Color},
    terminal::{self, ClearType},
};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const MAX_MSG_CHARS: usize = 3000; // per message stored for searching
const MIN_SCORE: f64 = 20.0; // hide weaker subsequence matches
const MAX_START_TRIES: usize = 25; // starting positions tried per match
const TITLE_LEN: usize = 200;

// 5-row block letters for the banner. 6 wide with a 2-column counter so
// letters like R, H and A stay legible.
const FONT: &[(char, [&str; 5])] = &[
    ('C', [" █████", "██    ", "██    ", "██    ", " █████"]),
    ('L', ["██    ", "██    ", "██    ", "██    ", "██████"]),
    ('A', [" ████ ", "██  ██", "██████", "██  ██", "██  ██"]),
    ('U', ["██  ██", "██  ██", "██  ██", "██  ██", " ████ "]),
    ('D', ["█████ ", "██  ██", "██  ██", "██  ██", "█████ "]),
    ('E', ["██████", "██    ", "█████ ", "██    ", "██████"]),
    ('S', [" █████", "██    ", " ████ ", "    ██", "█████ "]),
    ('R', ["█████ ", "██  ██", "█████ ", "██  ██", "██  ██"]),
    ('H', ["██  ██", "██  ██", "██████", "██  ██", "██  ██"]),
];

const ORANGE: Color = Color::Rgb { r: 217, g: 119, b: 87 };
const SEL_BG: Color = Color::Rgb { r: 45, g: 45, b: 55 };

// ------------------------------------------------------------------ model

#[derive(Clone)]
struct Session {
    id: String,
    title: String,
    first_prompt: String,
    cwd: String,
    ts: String,
    messages: Vec<String>,
}

struct Hit {
    idx: usize, // index into the sessions vector
    score: Option<f64>,
    snippet: String,
}

// ------------------------------------------------------------- indexing

fn extract_text(content: &serde_json::Value) -> Vec<String> {
    match content {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_session(path: &Path) -> io::Result<Session> {
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let reader = BufReader::new(File::open(path)?);

    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut ts: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut messages: Vec<String> = Vec::new();

    let mut buf = Vec::new();
    let mut lines = reader;
    loop {
        buf.clear();
        if lines.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let kind = obj["type"].as_str().unwrap_or("");
        match kind {
            "custom-title" => {
                if let Some(t) = obj["customTitle"].as_str().filter(|t| !t.is_empty()) {
                    custom_title = Some(t.to_string());
                }
            }
            "ai-title" => {
                if let Some(t) = obj["aiTitle"].as_str().filter(|t| !t.is_empty()) {
                    ai_title = Some(t.to_string());
                }
            }
            "user" | "assistant" => {
                if cwd.is_none() {
                    cwd = obj["cwd"].as_str().map(str::to_string);
                }
                if ts.is_none() {
                    ts = obj["timestamp"].as_str().map(str::to_string);
                }
                if obj["isMeta"].as_bool() == Some(true) {
                    continue;
                }
                for text in extract_text(&obj["message"]["content"]) {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if first_prompt.is_none() && kind == "user" && !text.starts_with('<') {
                        first_prompt = Some(text.to_string());
                    }
                    messages.push(text.chars().take(MAX_MSG_CHARS).collect());
                }
            }
            _ => {}
        }
    }

    let title = custom_title
        .or(ai_title)
        .or_else(|| first_prompt.clone())
        .unwrap_or_else(|| "(untitled)".to_string());
    Ok(Session {
        id,
        title: collapse_ws(&title).chars().take(TITLE_LEN).collect(),
        first_prompt: collapse_ws(first_prompt.as_deref().unwrap_or(""))
            .chars()
            .take(TITLE_LEN)
            .collect(),
        cwd: cwd.unwrap_or_else(|| "(unknown folder)".to_string()),
        ts: ts.unwrap_or_default(),
        messages,
    })
}

fn projects_dir() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn load_sessions() -> io::Result<Vec<Session>> {
    let root = projects_dir();
    let mut sessions = Vec::new();
    for project in fs::read_dir(&root)? {
        let project = project?.path();
        if !project.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&project)? {
            let path = entry?.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                match parse_session(&path) {
                    Ok(s) => sessions.push(s),
                    Err(e) => eprintln!("skipping {}: {e}", path.display()),
                }
            }
        }
    }
    // ISO timestamps sort correctly as strings; newest first.
    sessions.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(sessions)
}

// ------------------------------------------------------------- matching

fn lower_chars(chars: &[char]) -> Vec<char> {
    chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect()
}

fn find_char(hay: &[char], c: char, from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].iter().position(|&x| x == c).map(|i| i + from)
}

/// Score `query` (lowercase) against `text`. Returns (score 0-100, position).
/// An exact substring scores 100 (96 inside a word). Otherwise the query must
/// appear as a subsequence; tighter spans and word-start matches score higher.
fn fuzzy_match(query: &[char], text: &[char], lower: &[char]) -> Option<(f64, usize)> {
    let m = query.len();
    if m == 0 || lower.len() < m {
        return None;
    }
    if let Some(i) = lower.windows(m).position(|w| w == query) {
        let boundary = i == 0 || !text[i - 1].is_alphanumeric();
        return Some((if boundary { 100.0 } else { 96.0 }, i));
    }

    let first = query[0];
    let mut best: Option<(usize, Vec<usize>)> = None; // (span, positions)
    let mut tries = 0;
    let mut start_opt = find_char(lower, first, 0);
    while let Some(start) = start_opt {
        if tries >= MAX_START_TRIES {
            break;
        }
        tries += 1;
        let mut positions = vec![start];
        let mut pos = start;
        let mut ok = true;
        for &c in &query[1..] {
            match find_char(lower, c, pos + 1) {
                Some(p) => {
                    pos = p;
                    positions.push(p);
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            break; // later starts have even less text left to match
        }
        let span = pos - start + 1;
        if best.as_ref().is_none_or(|b| span < b.0) {
            best = Some((span, positions));
        }
        if span == m {
            break;
        }
        start_opt = find_char(lower, first, start + 1);
    }

    let (span, positions) = best?;
    let density = m as f64 / span as f64;
    let word_starts = positions
        .iter()
        .filter(|&&p| p == 0 || !text[p - 1].is_alphanumeric())
        .count() as f64
        / m as f64;
    let score = 90.0 * (0.75 * density + 0.25 * word_starts);
    if score < MIN_SCORE {
        return None;
    }
    Some((score, positions[0]))
}

fn snippet_around(text: &[char], pos: usize) -> String {
    const WIDTH: usize = 110;
    let start = pos.saturating_sub(35);
    let end = (start + WIDTH).min(text.len());
    let seg: String = text[start..end].iter().collect();
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.push_str(&collapse_ws(&seg));
    if end < text.len() {
        out.push_str("...");
    }
    out
}

fn search(sessions: &[Session], raw_query: &str) -> Vec<Hit> {
    let query: Vec<char> = raw_query.trim().to_lowercase().chars().collect();
    if query.is_empty() {
        return sessions
            .iter()
            .enumerate()
            .map(|(idx, s)| Hit {
                idx,
                score: None,
                snippet: if s.first_prompt.is_empty() {
                    s.title.clone()
                } else {
                    s.first_prompt.clone()
                },
            })
            .collect();
    }

    let mut results = Vec::new();
    for (idx, session) in sessions.iter().enumerate() {
        // (score, chars of the text it matched, match position)
        let mut best: Option<(f64, Vec<char>, usize)> = None;

        let title: Vec<char> = session.title.chars().collect();
        let title_lower = lower_chars(&title);
        if let Some((score, pos)) = fuzzy_match(&query, &title, &title_lower) {
            best = Some(((score + 5.0).min(100.0), title, pos));
        }
        for message in &session.messages {
            let chars: Vec<char> = message.chars().collect();
            let lower = lower_chars(&chars);
            if let Some((score, pos)) = fuzzy_match(&query, &chars, &lower) {
                if best.as_ref().is_none_or(|b| score > b.0) {
                    best = Some((score, chars, pos));
                }
            }
        }
        if let Some((score, chars, pos)) = best {
            results.push(Hit {
                idx,
                score: Some(score),
                snippet: snippet_around(&chars, pos),
            });
        }
    }
    // Stable sort: ties keep the newest-first session order.
    results.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

// -------------------------------------------------------------- display

fn tilde(path: &str) -> String {
    let home = home_dir().to_string_lossy().into_owned();
    match path.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// Truncate to `width` characters, adding "..." when cut.
fn fit(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        return text.to_string();
    }
    if width <= 3 {
        return text.chars().take(width).collect();
    }
    let mut s: String = text.chars().take(width - 3).collect();
    s.push_str("...");
    s
}

/// Truncate keeping the end visible (for paths).
fn fit_left(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return chars[chars.len() - width..].iter().collect();
    }
    let tail: String = chars[chars.len() - (width - 3)..].iter().collect();
    format!("...{tail}")
}

fn banner_rows(word: &str) -> Vec<String> {
    let mut rows = vec![String::new(); 5];
    for ch in word.chars() {
        let glyph = FONT
            .iter()
            .find(|(c, _)| *c == ch)
            .map(|(_, g)| g)
            .expect("letter in font");
        for (i, row) in rows.iter_mut().enumerate() {
            row.push_str(glyph[i]);
            row.push(' ');
        }
    }
    rows.into_iter().map(|r| r.trim_end().to_string()).collect()
}


#[derive(Clone, Copy, Default)]
struct Sty {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
    reverse: bool,
}

fn put(out: &mut impl Write, row: u16, col: u16, text: &str, sty: Sty) -> io::Result<()> {
    queue!(out, cursor::MoveTo(col, row))?;
    if let Some(c) = sty.fg {
        queue!(out, style::SetForegroundColor(c))?;
    }
    if let Some(c) = sty.bg {
        queue!(out, style::SetBackgroundColor(c))?;
    }
    if sty.bold {
        queue!(out, style::SetAttribute(Attribute::Bold))?;
    }
    if sty.dim {
        queue!(out, style::SetAttribute(Attribute::Dim))?;
    }
    if sty.reverse {
        queue!(out, style::SetAttribute(Attribute::Reverse))?;
    }
    queue!(out, style::Print(text))?;
    queue!(out, style::SetAttribute(Attribute::Reset), style::ResetColor)?;
    Ok(())
}

struct App {
    sessions: Vec<Session>,
    query: String,
    results: Vec<Hit>,
    selected: usize,
    top: usize,
}

impl App {
    fn new(sessions: Vec<Session>, query: String) -> Self {
        let results = search(&sessions, &query);
        App {
            sessions,
            query,
            results,
            selected: 0,
            top: 0,
        }
    }

    fn requery(&mut self) {
        self.results = search(&self.sessions, &self.query);
        self.selected = 0;
        self.top = 0;
    }

    fn selected_session(&self) -> Option<&Session> {
        self.results
            .get(self.selected)
            .map(|h| &self.sessions[h.idx])
    }
}

/// Draws the whole screen. Returns the cursor position and how many result
/// items fit, so PageUp/PageDown move by a real page.
fn draw(
    out: &mut impl Write,
    app: &App,
    cols: u16,
    rows: u16,
) -> io::Result<(u16, u16, usize)> {
    let width = cols as usize;
    let fg = |c: Color| Sty {
        fg: Some(c),
        ..Sty::default()
    };
    let dim = Sty {
        dim: true,
        ..Sty::default()
    };
    let border = fg(Color::DarkGrey);

    queue!(out, terminal::Clear(ClearType::All))?;
    let mut row: u16 = 0;

    // Banner: CLAUDE in orange, SEARCH in bold white.
    let claude = banner_rows("CLAUDE");
    let search_word = banner_rows("SEARCH");
    let claude_w = claude[0].chars().count();
    let gap = 4;
    if width >= claude_w * 2 + gap + 4 {
        for (a, b) in claude.iter().zip(&search_word) {
            put(out, row, 2, a, Sty { bold: true, ..fg(ORANGE) })?;
            put(out, row, (2 + claude_w + gap) as u16, b, Sty { bold: true, ..fg(Color::White) })?;
            row += 1;
        }
    } else {
        put(out, row, 2, "CLAUDE", Sty { bold: true, ..fg(ORANGE) })?;
        put(out, row, 9, "SEARCH", Sty { bold: true, ..fg(Color::White) })?;
        row += 1;
    }
    let total_msgs: usize = app.sessions.iter().map(|s| s.messages.len()).sum();
    let tagline = format!("{} chats · {} messages · offline", app.sessions.len(), total_msgs);
    put(out, row, 2, &tagline, dim)?;
    row += 2;

    // Search box
    let box_w = width.saturating_sub(4).clamp(20, 100);
    let inner = box_w - 2;
    let prompt = " ❯ ";
    let prompt_w = 3;
    let avail = inner.saturating_sub(prompt_w + 1);
    let qchars: Vec<char> = app.query.chars().collect();
    let field: String = if qchars.len() > avail {
        qchars[qchars.len() - avail..].iter().collect()
    } else {
        app.query.clone()
    };
    let bar = "─".repeat(inner);
    put(out, row, 2, &format!("╭{bar}╮"), border)?;
    let input_row = row + 1;
    put(out, input_row, 2, "│", border)?;
    put(out, input_row, 3, prompt, Sty { bold: true, ..fg(ORANGE) })?;
    if field.is_empty() {
        put(out, input_row, (3 + prompt_w) as u16, "type to search…", dim)?;
    } else {
        put(out, input_row, (3 + prompt_w) as u16, &field, Sty { bold: true, ..Sty::default() })?;
    }
    put(out, input_row, (2 + box_w - 1) as u16, "│", border)?;
    put(out, row + 2, 2, &format!("╰{bar}╯"), border)?;
    let cursor_x = (3 + prompt_w + field.chars().count()).min(width.saturating_sub(2));
    row += 4;

    // Result count
    let n = app.results.len();
    let count = if app.query.trim().is_empty() {
        format!("{n} chats")
    } else {
        format!("{n} {}", if n == 1 { "match" } else { "matches" })
    };
    put(out, row, 2, &count, Sty { bold: true, ..Sty::default() })?;
    if app.query.trim().is_empty() {
        put(out, row, (2 + count.len()) as u16, "  · most recent first", dim)?;
    }
    row += 2;

    // Results: two rows per item.
    //   ▌ 87  Title of the chat                         ~/folder
    //         matching snippet...                      2026-11-17
    let footer_row = rows.saturating_sub(1);
    let list_top = row;
    let visible = ((footer_row.saturating_sub(1) as usize).saturating_sub(list_top as usize) / 2).max(1);
    let mut top = app.top;
    if app.selected < top {
        top = app.selected;
    }
    if app.selected >= top + visible {
        top = app.selected + 1 - visible;
    }

    let folder_w = (width / 3).clamp(16, 40);
    let title_w = width.saturating_sub(folder_w + 12).max(10);
    let end = (top + visible).min(n);
    for idx in top..end {
        let hit = &app.results[idx];
        let session = &app.sessions[hit.idx];
        let y = list_top + ((idx - top) * 2) as u16;
        let is_sel = idx == app.selected;
        let bg = if is_sel { Some(SEL_BG) } else { None };
        let s = |base: Sty| Sty { bg, ..base };

        let pad = " ".repeat(width);
        put(out, y, 0, &pad, s(Sty::default()))?;
        put(out, y + 1, 0, &pad, s(Sty::default()))?;
        if is_sel {
            put(out, y, 1, "▌", s(fg(ORANGE)))?;
            put(out, y + 1, 1, "▌", s(fg(ORANGE)))?;
        }

        if let Some(score) = hit.score {
            put(out, y, 3, &format!("{score:>3.0}"), s(fg(Color::Green)))?;
        }
        let title = fit(&session.title, title_w);
        put(out, y, 8, &title, s(Sty { bold: true, ..Sty::default() }))?;
        let folder = fit_left(&tilde(&session.cwd), folder_w);
        let folder_col = width.saturating_sub(folder.chars().count() + 2) as u16;
        put(out, y, folder_col, &folder, s(fg(Color::Blue)))?;

        let date = session.ts.get(..10).unwrap_or("");
        let snippet = fit(&hit.snippet, width.saturating_sub(8 + date.len() + 4).max(10));
        put(out, y + 1, 8, &snippet, s(fg(Color::Grey)))?;
        let date_col = width.saturating_sub(date.len() + 2) as u16;
        put(out, y + 1, date_col, date, s(dim))?;
    }

    if n == 0 {
        put(out, list_top, 4, "No matches — try fewer or different words.", dim)?;
    }

    // Footer: key hints only.
    let mut col: u16 = 2;
    for (key, what) in [("↑↓", "move"), ("enter", "resume"), ("^U", "clear"), ("esc", "quit")] {
        if col as usize + key.len() + what.len() + 4 > width {
            break;
        }
        put(out, footer_row, col, key, Sty { bold: true, ..fg(ORANGE) })?;
        col += key.chars().count() as u16 + 1;
        put(out, footer_row, col, what, dim)?;
        col += what.len() as u16 + 3;
    }

    Ok((cursor_x as u16, input_row, visible))
}

// ------------------------------------------------------------ interactive

/// Restores the terminal on drop, including on error paths.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        queue!(out, terminal::EnterAlternateScreen, cursor::Show)?;
        out.flush()?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = queue!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

enum Action {
    Continue,
    Quit,
    Resume,
}

fn handle_key(app: &mut App, key: KeyEvent, visible: usize) -> Action {
    let last = app.results.len().saturating_sub(1);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => return Action::Quit,
        KeyCode::Char('c') if ctrl => return Action::Quit,
        KeyCode::Up => app.selected = app.selected.saturating_sub(1),
        KeyCode::Down => app.selected = (app.selected + 1).min(last),
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(visible),
        KeyCode::PageDown => app.selected = (app.selected + visible).min(last),
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = last,
        KeyCode::Enter => {
            if app.selected_session().is_some() {
                return Action::Resume;
            }
        }
        KeyCode::Backspace => {
            app.query.pop();
            app.requery();
        }
        KeyCode::Char('u') if ctrl => {
            app.query.clear();
            app.requery();
        }
        KeyCode::Char(c) if !ctrl => {
            app.query.push(c);
            app.requery();
        }
        _ => {}
    }
    Action::Continue
}

/// Runs the UI. Returns the session chosen with Enter, if any.
fn run_ui(app: &mut App) -> io::Result<Option<Session>> {
    let _guard = TerminalGuard::enter()?;
    let mut out = io::stdout();
    loop {
        let (cols, rows) = terminal::size()?;
        let (cx, cy, visible) = draw(&mut out, app, cols, rows)?;
        queue!(out, cursor::MoveTo(cx, cy))?;
        out.flush()?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(app, key, visible) {
                Action::Continue => {}
                Action::Quit => return Ok(None),
                Action::Resume => return Ok(app.selected_session().cloned()),
            }
        }
    }
}

/// Replace this process with `claude --resume <id>` in the chat's folder.
fn resume(session: &Session) -> io::Error {
    let cwd = PathBuf::from(&session.cwd);
    if !cwd.is_dir() {
        return io::Error::new(
            io::ErrorKind::NotFound,
            format!("folder no longer exists: {}", session.cwd),
        );
    }
    eprintln!(
        "Resuming {:?} in {} ...",
        session.title,
        tilde(&session.cwd)
    );
    // exec() only returns on failure.
    Command::new("claude")
        .arg("--resume")
        .arg(&session.id)
        .current_dir(&cwd)
        .exec()
}

// --------------------------------------------------------------- main

fn print_results(sessions: &[Session], query: &str) {
    let results = search(sessions, query);
    println!("{} match(es) for {:?}", results.len(), query);
    for hit in results.iter().take(20) {
        let s = &sessions[hit.idx];
        let score = hit
            .score
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "--".to_string());
        println!("{score:>4}  {}", s.title);
        println!(
            "      folder: {}   date: {}   id: {}",
            tilde(&s.cwd),
            s.ts.get(..10).unwrap_or(""),
            s.id
        );
        println!("      {}", hit.snippet);
    }
}

fn main() {
    let mut print_mode = false;
    let mut words = Vec::new();
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--print" => print_mode = true,
            "-h" | "--help" => {
                println!("usage: csearch [--print] [query...]");
                return;
            }
            _ => words.push(arg),
        }
    }
    let query = words.join(" ");

    if !projects_dir().is_dir() {
        eprintln!("No Claude projects folder found at {}", projects_dir().display());
        process::exit(1);
    }
    let sessions = match load_sessions() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read sessions: {e}");
            process::exit(1);
        }
    };

    if print_mode {
        print_results(&sessions, &query);
        return;
    }

    let mut app = App::new(sessions, query);
    match run_ui(&mut app) {
        Ok(Some(session)) => {
            // Terminal is restored by now; hand the window over to claude.
            let err = resume(&session);
            eprintln!("csearch: {err}");
            process::exit(1);
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("csearch: terminal error: {e}");
            process::exit(1);
        }
    }
}
