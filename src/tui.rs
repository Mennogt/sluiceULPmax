// tui.rs — интерфейс как в Node-версии: пять экранов, кнопки, мышь, цвета.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{
    human, open_sequential, parse_cred, sanitize, start_scan, strip_cr, Engine, Prog, ScanPlan,
    VERSION,
};

// ─────────────────────────────────────────────────────────── цвета

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[38;5;240m";
const GRAY: &str = "\x1b[38;5;245m";
const WHITE: &str = "\x1b[38;5;252m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[38;5;45m";
const BLUE: &str = "\x1b[38;5;39m";
const GREEN: &str = "\x1b[38;5;114m";
const YELLOW: &str = "\x1b[38;5;221m";
const MAGENTA: &str = "\x1b[38;5;170m";
const INV: &str = "\x1b[7m";

fn no_color() -> bool {
    static ONCE: AtomicBool = AtomicBool::new(false);
    ONCE.load(Ordering::Relaxed)
}
pub fn set_plain() {
    static SET: AtomicBool = AtomicBool::new(false);
    if !SET.swap(true, Ordering::Relaxed) {}
}

// ─────────────────────────────────────────────────────────── консоль

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Coord {
        pub x: i16,
        pub y: i16,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Rect {
        pub left: i16,
        pub top: i16,
        pub right: i16,
        pub bottom: i16,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Csbi {
        pub size: Coord,
        pub cursor: Coord,
        pub attrs: u16,
        pub window: Rect,
        pub max: Coord,
    }

    extern "system" {
        pub fn GetStdHandle(id: u32) -> *mut c_void;
        pub fn GetConsoleMode(h: *mut c_void, m: *mut u32) -> i32;
        pub fn SetConsoleMode(h: *mut c_void, m: u32) -> i32;
        pub fn GetConsoleScreenBufferInfo(h: *mut c_void, info: *mut Csbi) -> i32;
    }

    pub const STD_INPUT: u32 = -10i32 as u32;
    pub const STD_OUTPUT: u32 = -11i32 as u32;
    pub const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    pub const ENABLE_LINE_INPUT: u32 = 0x0002;
    pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
    pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    pub const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
}

#[cfg(windows)]
fn term_size() -> (usize, usize) {
    unsafe {
        let h = win::GetStdHandle(win::STD_OUTPUT);
        let mut info = win::Csbi::default();
        if win::GetConsoleScreenBufferInfo(h, &mut info) != 0 {
            let w = (info.window.right - info.window.left + 1).max(20) as usize;
            let hh = (info.window.bottom - info.window.top + 1).max(10) as usize;
            return (w, hh);
        }
    }
    (100, 30)
}

#[cfg(not(windows))]
fn term_size() -> (usize, usize) {
    (100, 30)
}

struct Screen {
    #[cfg(windows)]
    in_mode: u32,
    #[cfg(windows)]
    out_mode: u32,
    #[cfg(windows)]
    want_in: u32,
    #[cfg(windows)]
    want_out: u32,
    active: bool,
}

impl Screen {
    fn enter() -> Screen {
        #[cfg(windows)]
        unsafe {
            let hin = win::GetStdHandle(win::STD_INPUT);
            let hout = win::GetStdHandle(win::STD_OUTPUT);
            let mut im = 0u32;
            let mut om = 0u32;
            win::GetConsoleMode(hin, &mut im);
            win::GetConsoleMode(hout, &mut om);
            let want_in = (im & !(win::ENABLE_LINE_INPUT | win::ENABLE_ECHO_INPUT | win::ENABLE_PROCESSED_INPUT))
                | win::ENABLE_VIRTUAL_TERMINAL_INPUT;
            let want_out = om | win::ENABLE_PROCESSED_OUTPUT | win::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
            let mut s = Screen { in_mode: im, out_mode: om, want_in, want_out, active: false };
            s.on();
            return s;
        }
        #[cfg(not(windows))]
        {
            let mut s = Screen { active: false };
            s.on();
            s
        }
    }
    fn on(&mut self) {
        #[cfg(windows)]
        unsafe {
            win::SetConsoleMode(win::GetStdHandle(win::STD_INPUT), self.want_in);
            win::SetConsoleMode(win::GetStdHandle(win::STD_OUTPUT), self.want_out);
        }
        print!("\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[2J");
        let _ = io::stdout().flush();
        self.active = true;
    }
    fn off(&mut self) {
        if !self.active {
            return;
        }
        print!("\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
        self.active = false;
        #[cfg(windows)]
        unsafe {
            win::SetConsoleMode(win::GetStdHandle(win::STD_INPUT), self.in_mode);
            win::SetConsoleMode(win::GetStdHandle(win::STD_OUTPUT), self.out_mode);
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.off();
    }
}

// ─────────────────────────────────────────────────────────── строки и ширина

fn vis_len(s: &str) -> usize {
    let mut n = 0;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // пропускаем CSI ... m/K
            for c2 in it.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            for c2 in it.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn fit(s: &str, w: usize) -> String {
    let n = vis_len(s);
    if n > w {
        let plain = strip_ansi(s);
        let mut t: String = plain.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        let mut t = s.to_string();
        t.push_str(&" ".repeat(w - n));
        t
    }
}

fn pad_vis(s: &str, w: usize) -> String {
    let n = vis_len(s);
    if n >= w {
        s.to_string()
    } else {
        let mut t = s.to_string();
        t.push_str(&" ".repeat(w - n));
        t
    }
}

fn center(s: &str, w: usize) -> String {
    let n = vis_len(s);
    if n >= w {
        return s.to_string();
    }
    format!("{}{}", " ".repeat((w - n) / 2), s)
}

fn trunc_mid(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= w || w < 8 {
        return s.chars().take(w).collect();
    }
    let keep = w - 1;
    let left = keep / 2;
    let right = keep - left;
    let mut out: String = chars[..left].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - right..]);
    out
}

fn plural(n: usize, one: &str, few: &str, many: &str) -> &'static str {
    let _ = n;
    // упрощённо, как в JS-версии
    if n % 10 == 1 && n % 100 != 11 {
        Box::leak(one.to_string().into_boxed_str())
    } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
        Box::leak(few.to_string().into_boxed_str())
    } else {
        Box::leak(many.to_string().into_boxed_str())
    }
}

// ─────────────────────────────────────────────────────────── события

#[derive(Debug, Clone)]
enum Ev {
    Char(char),
    Text(String),
    Enter,
    Tab,
    BackTab,
    Esc,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    CtrlC,
    CtrlV,
    Mouse(u16, u16),
}

fn spawn_reader(tx: std::sync::mpsc::Sender<Ev>) {
    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 4096];
        let mut acc: Vec<u8> = Vec::new();
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            acc.extend_from_slice(&buf[..n]);
            // разбор накопленного
            loop {
                if acc.is_empty() {
                    break;
                }
                if acc[0] == 0x1b {
                    // мышь SGR
                    if acc.len() >= 3 && acc[1] == b'[' && acc[2] == b'<' {
                        if let Some(end) = acc.iter().position(|&b| b == b'M' || b == b'm') {
                            let seq = String::from_utf8_lossy(&acc[3..end]).to_string();
                            let parts: Vec<&str> = seq.split(';').collect();
                            if parts.len() == 3 && acc[end] == b'M' {
                                let b: u16 = parts[0].parse().unwrap_or(99);
                                let x: u16 = parts[1].parse::<u16>().unwrap_or(1).saturating_sub(1);
                                let y: u16 = parts[2].parse::<u16>().unwrap_or(1).saturating_sub(1);
                                if b & 0b11 == 0 {
                                    let _ = tx.send(Ev::Mouse(x, y));
                                }
                            }
                            acc.drain(..=end);
                            continue;
                        }
                        break;
                    }
                    // служебные последовательности
                    if acc.len() >= 2 && acc[1] == b'[' {
                        if acc.len() < 3 {
                            break;
                        }
                        let (ev, used) = match (acc[2], acc.get(3).copied()) {
                            (b'A', _) => (Some(Ev::Up), 3),
                            (b'B', _) => (Some(Ev::Down), 3),
                            (b'C', _) => (Some(Ev::Right), 3),
                            (b'D', _) => (Some(Ev::Left), 3),
                            (b'Z', _) => (Some(Ev::BackTab), 3),
                            (b'H', _) => (Some(Ev::Home), 3),
                            (b'F', _) => (Some(Ev::End), 3),
                            (b'1', Some(b'~')) => (Some(Ev::Home), 4),
                            (b'3', Some(b'~')) => (Some(Ev::Delete), 4),
                            (b'4', Some(b'~')) => (Some(Ev::End), 4),
                            (b'7', Some(b'~')) => (Some(Ev::Home), 4),
                            (b'8', Some(b'~')) => (Some(Ev::End), 4),
                            _ => (None, 1),
                        };
                        if let Some(ev) = ev {
                            acc.drain(..used);
                            let _ = tx.send(ev);
                            continue;
                        }
                        // неизвестная — считаем Esc
                        acc.drain(..1);
                        let _ = tx.send(Ev::Esc);
                        continue;
                    }
                    if acc.len() == 1 {
                        // одиночный Esc — подождём ещё чуть-чуть
                        break;
                    }
                    acc.drain(..1);
                    let _ = tx.send(Ev::Esc);
                    continue;
                }
                let b = acc[0];
                match b {
                    b'\r' | b'\n' => {
                        acc.drain(..1);
                        let _ = tx.send(Ev::Enter);
                    }
                    0x7f | 0x08 => {
                        acc.drain(..1);
                        let _ = tx.send(Ev::Backspace);
                    }
                    b'\t' => {
                        acc.drain(..1);
                        let _ = tx.send(Ev::Tab);
                    }
                    0x03 => {
                        acc.drain(..1);
                        let _ = tx.send(Ev::CtrlC);
                    }
                    0x16 => {
                        acc.drain(..1);
                        let _ = tx.send(Ev::CtrlV);
                    }
                    _ => {
                        // кусок печатных байт (возможно UTF-8)
                        let mut end = 0usize;
                        while end < acc.len() && acc[end] >= 0x20 && acc[end] != 0x7f {
                            end += 1;
                        }
                        if end == 0 {
                            acc.drain(..1);
                            continue;
                        }
                        if end == acc.len() && end < acc.len() + 1 && acc.len() < 4096 {
                            // может, символ ещё не целиком — но для UTF-8 проверяем хвост
                        }
                        let chunk = &acc[..end];
                        let s = String::from_utf8_lossy(chunk).to_string();
                        acc.drain(..end);
                        let mut chars = s.chars();
                        if let (Some(c), None) = (chars.next(), chars.next()) {
                            let _ = tx.send(Ev::Char(c));
                        } else {
                            let _ = tx.send(Ev::Text(s));
                        }
                    }
                }
            }
        }
    });
}

// ─────────────────────────────────────────────────────────── состояние

#[derive(PartialEq, Clone, Copy)]
enum Format {
    Full,
    Login,
}

struct PathItem {
    root: String,
    files: Vec<PathBuf>,
    bytes: u64,
    is_dir: bool,
}

struct Browser {
    dir: PathBuf,
    entries: Vec<(String, PathBuf, bool, u64)>, // имя, путь, папка, размер
    idx: usize,
    target: Target,
}

#[derive(PartialEq, Clone, Copy)]
enum Target {
    Paths,
    Queries,
    OutDir,
}

struct Run {
    prog: Arc<Prog>,
    handle: Option<std::thread::JoinHandle<io::Result<crate::ScanOut>>>,
    started: Instant,
    names: Vec<String>,
    engine: Arc<Engine>,
    out_dir: String,
    format: Format,
    count_only: bool,
    threads: usize,
    live: bool,
}

struct Res {
    counts: Vec<u64>,
    patterns: Vec<String>,
    hits: usize,
    bytes: u64,
    secs: f64,
    files: usize,
    out_dir: String,
}

struct App {
    step: usize,
    paths: Vec<PathItem>,
    sel: usize,
    focus: usize,
    query: String,
    qcur: usize,
    format: Format,
    out_dir: String,
    count_only: bool,
    modal: Option<Browser>,
    msg: String,
    dirty: bool,
    run: Option<Run>,
    res: Option<Res>,
    last_dir: PathBuf,
    buttons: Vec<(u16, u16, u16, &'static str)>,
    rows: Vec<(u16, usize)>,
}

impl App {
    fn new() -> App {
        App {
            step: 0,
            paths: Vec::new(),
            sel: 0,
            focus: 3,
            query: String::new(),
            qcur: 0,
            format: Format::Full,
            out_dir: "hits".into(),
            count_only: false,
            modal: None,
            msg: String::new(),
            dirty: true,
            run: None,
            res: None,
            last_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\")),
            buttons: Vec::new(),
            rows: Vec::new(),
        }
    }

    fn queries(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for part in self.query.split(|c| c == '|' || c == '\n' || c == '\r') {
            let t = part.trim();
            if !t.is_empty() && !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
        out
    }

    fn total_bytes(&self) -> u64 {
        self.paths.iter().map(|p| p.bytes).sum()
    }
    fn total_files(&self) -> usize {
        self.paths.iter().map(|p| p.files.len()).sum()
    }

    fn add_path(&mut self, p: &str) {
        let path = Path::new(p);
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                self.msg = format!("не найдено: {}", p);
                return;
            }
        };
        let (files, bytes) = if meta.is_dir() {
            let list = list_files(path);
            let b = list.iter().filter_map(|f| fs::metadata(f).ok().map(|m| m.len())).sum();
            (list, b)
        } else {
            (vec![path.to_path_buf()], meta.len())
        };
        if files.is_empty() {
            self.msg = format!("нет .txt по пути {}", p);
            return;
        }
        self.paths.push(PathItem { root: p.to_string(), files, bytes, is_dir: meta.is_dir() });
        self.msg = format!("добавлено: {}", p);
    }
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x.eq_ignore_ascii_case("txt")).unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
    out
}

// ─────────────────────────────────────────────────────────── отрисовка

struct Btn {
    label: String,
    action: &'static str,
}

fn draw_buttons(list: &[Btn], focus: usize, y: usize, w: usize, app: &mut App) -> String {
    let cell = list.iter().map(|b| b.label.chars().count()).max().unwrap_or(8) + 4;
    let total = cell * list.len() + 2 * (list.len().saturating_sub(1));
    let left = w.saturating_sub(total) / 2;
    let mut line = " ".repeat(left);
    let mut x = left;
    for (i, b) in list.iter().enumerate() {
        let inner = pad_vis(&b.label, cell - 2);
        if i == focus {
            line.push_str(&format!("{} {} {}", INV, inner, RESET));
        } else {
            line.push_str(&format!("{}[{}]{}", DIM, inner, RESET));
        }
        app.buttons.push((x as u16, y as u16, cell as u16, b.action));
        x += cell + 2;
        line.push_str("  ");
    }
    line
}

fn header(w: usize, step: usize) -> Vec<String> {
    let mut v = Vec::new();
    v.push(String::new());
    v.push(center(&format!("{}S L U I C E U L P{}", if no_color() { "" } else { CYAN }, if no_color() { "" } else { RESET }), w));
    v.push(center(&format!("{}один проход · сколько угодно строк · твой диск{}", if no_color() { "" } else { DIM }, if no_color() { "" } else { RESET }), w));
    v.push(String::new());
    let names = ["источники", "запросы", "вывод", "прогон", "итоги"];
    let mut crumbs = String::new();
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            crumbs.push_str(&format!(" {}·{} ", DIM, RESET));
        }
        if i == step {
            crumbs.push_str(&format!("{}{} {}{}", INV, i + 1, n, RESET));
        } else {
            crumbs.push_str(&format!("{}{} {}{}", DIM, i + 1, n, RESET));
        }
    }
    v.push(center(&crumbs, w));
    v.push(format!("  {}{}{}", DIM, "━".repeat(w.saturating_sub(3)), RESET));
    v.push(String::new());
    v
}

fn footer(w: usize, hint: &str) -> Vec<String> {
    vec![
        String::new(),
        format!("  {}{}{}", DIM, "─".repeat(w.saturating_sub(3)), RESET),
        format!("  {}{}{}", DIM, hint, RESET),
    ]
}

fn render(lines: &[String], w: usize, h: usize) {
    let mut out = String::from("\x1b[H");
    for i in 0..h {
        let l = lines.get(i).map(|s| s.as_str()).unwrap_or("");
        out.push_str(&fit(l, w));
        out.push_str("\x1b[K");
        if i + 1 < h {
            out.push_str("\r\n");
        }
    }
    let mut so = io::stdout();
    let _ = so.write_all(out.as_bytes());
    let _ = so.flush();
}

// ─────────────────────────────────────────────────────────── main loop

pub fn run() -> io::Result<()> {
    let mut screen = Screen::enter();
    let (tx, rx) = channel();
    spawn_reader(tx);
    let mut app = App::new();

    loop {
        // события
        loop {
            match rx.try_recv() {
                Ok(ev) => {
                    if handle_ev(&mut app, ev, &mut screen)? {
                        return Ok(());
                    }
                    app.dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // завершение скана
        if let Some(run) = &app.run {
            if let Some(h) = &run.handle {
                if h.is_finished() {
                    let mut run = app.run.take().unwrap();
                    let out = run.handle.take().unwrap().join().unwrap()?;
                    let matched = out.hits.len();
                    let files = if run.count_only { 0 } else { write_results(&run, &out)? };
                    app.res = Some(Res {
                        counts: out.counts,
                        patterns: run.names.clone(),
                        hits: matched,
                        bytes: out.bytes,
                        secs: out.secs,
                        files,
                        out_dir: run.out_dir.clone(),
                    });
                    app.step = 4;
                    app.focus = 0;
                    app.dirty = true;
                }
            }
        }

        if app.dirty || app.step == 3 {
            app.dirty = false;
            app.buttons.clear();
            app.rows.clear();
            let (w, h) = term_size();
            let lines = screen_for(&mut app, w);
            render(&lines, w, h);
        }

        std::thread::sleep(Duration::from_millis(70));
    }
}

fn handle_ev(app: &mut App, ev: Ev, screen: &mut Screen) -> io::Result<bool> {
    if let Ev::CtrlC = ev {
        return Ok(true);
    }
    app.dirty = true;

    // ── мышь: сначала кнопки, потом строки списка
    if let Ev::Mouse(x, y) = ev {
        if app.modal.is_some() {
            return Ok(false);
        }
        for (bx, by, bw, action) in app.buttons.clone() {
            if y == by && x >= bx && x < bx + bw {
                return Ok(activate(app, action, screen)?);
            }
        }
        for (ry, idx) in app.rows.clone() {
            if y == ry {
                app.sel = idx;
                return Ok(false);
            }
        }
        return Ok(false);
    }

    if app.step == 3 {
        if matches!(ev, Ev::Esc) {
            if let Some(run) = &app.run {
                run.prog.stop.store(true, Ordering::Relaxed);
            }
        }
        return Ok(false);
    }
    if app.modal.is_some() {
        browser_key(app, ev);
        return Ok(false);
    }
    if app.step == 1 {
        match ev {
            Ev::Char(c) => {
                let cur = app.qcur.min(app.query.chars().count());
                let mut chars: Vec<char> = app.query.chars().collect();
                chars.insert(cur, c);
                app.query = chars.into_iter().collect();
                app.qcur = cur + 1;
            }
            Ev::Text(s) => {
                let cur = app.qcur.min(app.query.chars().count());
                let mut chars: Vec<char> = app.query.chars().collect();
                for (k, ch) in s.chars().enumerate() {
                    chars.insert(cur + k, ch);
                }
                app.query = chars.into_iter().collect();
                app.qcur = cur + s.chars().count();
            }
            Ev::Backspace => {
                let cur = app.qcur.min(app.query.chars().count());
                if cur > 0 {
                    let mut chars: Vec<char> = app.query.chars().collect();
                    chars.remove(cur - 1);
                    app.query = chars.into_iter().collect();
                    app.qcur = cur - 1;
                }
            }
            Ev::Delete => {
                let cur = app.qcur.min(app.query.chars().count());
                let mut chars: Vec<char> = app.query.chars().collect();
                if cur < chars.len() {
                    chars.remove(cur);
                    app.query = chars.into_iter().collect();
                }
            }
            Ev::Left => app.qcur = app.qcur.saturating_sub(1),
            Ev::Right => app.qcur = (app.qcur + 1).min(app.query.chars().count()),
            Ev::Home => app.qcur = 0,
            Ev::End => app.qcur = app.query.chars().count(),
            Ev::Enter | Ev::Tab => next(app),
            Ev::Esc => back(app),
            Ev::CtrlV => paste_clipboard(app, screen)?,
            _ => {}
        }
        return Ok(false);
    }

    // остальные экраны: кнопки в фокусе + список
    match ev {
        Ev::Tab | Ev::Right => {
            let n = app.buttons.len().max(1);
            app.focus = (app.focus + 1) % n;
        }
        Ev::BackTab | Ev::Left => {
            let n = app.buttons.len().max(1);
            app.focus = (app.focus + n - 1) % n;
        }
        Ev::Up => app.sel = app.sel.saturating_sub(1),
        Ev::Down => app.sel = (app.sel + 1).min(app.paths.len().saturating_sub(1)),
        Ev::Delete => {
            if app.step == 0 && !app.paths.is_empty() {
                app.paths.remove(app.sel);
                app.sel = app.sel.min(app.paths.len().saturating_sub(1));
            }
        }
        Ev::Enter => {
            if let Some((_, _, _, action)) = app.buttons.get(app.focus).cloned() {
                return Ok(activate(app, action, screen)?);
            }
        }
        Ev::Char(c) => match c.to_ascii_lowercase() {
            'q' => return Ok(true),
            'a' if app.step == 0 => return Ok(activate(app, "addFile", screen)?),
            'd' if app.step == 0 => return Ok(activate(app, "addFolder", screen)?),
            'f' if app.step == 2 => {
                app.format = if app.format == Format::Full { Format::Login } else { Format::Full };
            }
            'c' if app.step == 2 => app.count_only = !app.count_only,
            'p' if app.step == 2 => return Ok(activate(app, "pickDir", screen)?),
            _ => {}
        },
        Ev::Text(s) => {
            if app.step == 0 {
                for line in s.split('\n') {
                    let t = line.trim().trim_matches('"');
                    if !t.is_empty() {
                        app.add_path(t);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn activate(app: &mut App, action: &str, screen: &mut Screen) -> io::Result<bool> {
    match action {
        "addFile" => {
            if let Some(p) = native_pick(screen, false) {
                for line in p.lines() {
                    let t = line.trim().trim_matches('"');
                    if !t.is_empty() {
                        app.add_path(t);
                    }
                }
            } else {
                open_browser(app, app.last_dir.clone(), Target::Paths);
            }
        }
        "addFolder" => {
            if let Some(p) = native_pick(screen, true) {
                for line in p.lines() {
                    let t = line.trim().trim_matches('"');
                    if !t.is_empty() {
                        app.add_path(t);
                    }
                }
            } else {
                open_browser(app, app.last_dir.clone(), Target::Paths);
            }
        }
        "remove" => {
            if !app.paths.is_empty() {
                app.paths.remove(app.sel);
                app.sel = app.sel.min(app.paths.len().saturating_sub(1));
            }
        }
        "next" => next(app),
        "back" => back(app),
        "toggleFormat" => {
            app.format = if app.format == Format::Full { Format::Login } else { Format::Full };
        }
        "toggleCount" => app.count_only = !app.count_only,
        "pickDir" => {
            if let Some(p) = native_pick(screen, true) {
                let t = p.lines().next().unwrap_or("").trim().trim_matches('"').to_string();
                if !t.is_empty() {
                    app.out_dir = t;
                    app.msg = format!("папка вывода: {}", app.out_dir);
                }
            } else {
                open_browser(app, std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\")), Target::OutDir);
            }
        }
        "queriesFile" => {
            if let Some(p) = native_pick(screen, false) {
                let t = p.lines().next().unwrap_or("").trim().trim_matches('"').to_string();
                if let Ok(text) = fs::read_to_string(&t) {
                    app.query = text.trim().to_string();
                    app.qcur = app.query.chars().count();
                    app.msg = format!("запросы из {}", t);
                }
            } else {
                open_browser(app, app.last_dir.clone(), Target::Queries);
            }
        }
        "clear" => {
            app.query.clear();
            app.qcur = 0;
        }
        "run" => start(app)?,
        "again" => {
            app.res = None;
            app.step = 0;
            app.focus = 3;
        }
        "quit" => return Ok(true),
        _ => {}
    }
    Ok(false)
}

fn next(app: &mut App) {
    if app.step == 0 && app.paths.is_empty() {
        app.msg = "добавь хотя бы один источник".into();
        return;
    }
    if app.step == 1 && app.queries().is_empty() {
        app.msg = "нужен хотя бы один запрос".into();
        return;
    }
    app.msg.clear();
    app.step = (app.step + 1).min(2);
    app.focus = if app.step == 2 { 4 } else { 0 };
}

fn back(app: &mut App) {
    app.msg.clear();
    app.step = app.step.saturating_sub(1);
}

fn start(app: &mut App) -> io::Result<()> {
    let names = app.queries();
    if names.is_empty() {
        app.msg = "нет запросов".into();
        return Ok(());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for p in &app.paths {
        files.extend(p.files.iter().cloned());
    }
    if files.is_empty() {
        app.msg = "нечего сканировать".into();
        return Ok(());
    }
    let total: u64 = files
        .iter()
        .filter_map(|f| fs::metadata(f).ok().map(|m| m.len()))
        .sum();
    let engine = Arc::new(Engine::build(&names, true, "filter"));
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = cores.max(2);
    let prog = Arc::new(Prog::new(engine.npat(), total));
    let handle = spawn_multi(Arc::clone(&engine), files, threads, 32, Arc::clone(&prog));

    app.run = Some(Run {
        prog,
        handle: Some(handle),
        started: Instant::now(),
        names: names.clone(),
        engine,
        out_dir: app.out_dir.clone(),
        format: app.format,
        count_only: app.count_only,
        threads,
        live: true,
    });
    app.step = 3;
    Ok(())
}

fn spawn_multi(
    engine: Arc<Engine>,
    files: Vec<PathBuf>,
    threads: usize,
    chunk: usize,
    prog: Arc<Prog>,
) -> std::thread::JoinHandle<io::Result<crate::ScanOut>> {
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut all: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut bytes = 0u64;
        for f in files {
            if prog.stop.load(Ordering::Relaxed) {
                break;
            }
            let meta = match fs::metadata(&f) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let fh = match open_sequential(&f) {
                Ok(x) => Arc::new(x),
                Err(_) => continue,
            };
            let per_file = ((meta.len() / (8 * 1024 * 1024)) as usize).max(1);
            let th = threads.max(1).min(per_file);
            let plan = ScanPlan::build(Arc::clone(&fh), meta.len(), th, chunk, engine.max_len().max(1), true)?;
            let h = start_scan(Arc::clone(&engine), plan, Arc::clone(&prog));
            let out = h.join().unwrap()?;
            bytes += out.bytes;
            all.extend(out.hits);
        }
        let counts = prog.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        Ok(crate::ScanOut { counts, hits: all, bytes, secs: t0.elapsed().as_secs_f64() })
    })
}

// Нативный диалог Windows. None — диалог недоступен (тогда открываем встроенный браузер),
// Some("") — пользователь отменил.
fn native_pick(screen: &mut Screen, dir_mode: bool) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let base = std::env::temp_dir().join(format!("sluiceulp-pick-{}-{}", std::process::id(), Instant::now().elapsed().as_nanos()));
    let out_path = base.with_extension("txt");
    let ps_path = base.with_extension("ps1");
    let init = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let script = if dir_mode {
        format!(
            "$ErrorActionPreference='Stop'\nAdd-Type -AssemblyName System.Windows.Forms\nAdd-Type -AssemblyName System.Drawing\n\
             [System.Windows.Forms.Application]::EnableVisualStyles()\n$out='{}'\n\
             $owner=New-Object System.Windows.Forms.Form\n$owner.TopMost=$true\n$owner.ShowInTaskbar=$false\n\
             $owner.StartPosition='CenterScreen'\n$owner.Size=New-Object System.Drawing.Size(1,1)\n$owner.Opacity=0\n$owner.Show()\n\
             $dlg=New-Object System.Windows.Forms.FolderBrowserDialog\n$dlg.Description='sluiceULPmax - choose folder'\n\
             $dlg.ShowNewFolderButton=$false\n$res='CANCEL'\n\
             if ($dlg.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) {{ $res=$dlg.SelectedPath }}\n\
             $dlg.Dispose()\n$owner.Close()\n[System.IO.File]::WriteAllText($out,$res,[System.Text.Encoding]::UTF8)\n",
            out_path.display().to_string().replace('\\', "\\\\")
        )
    } else {
        format!(
            "$ErrorActionPreference='Stop'\nAdd-Type -AssemblyName System.Windows.Forms\nAdd-Type -AssemblyName System.Drawing\n\
             [System.Windows.Forms.Application]::EnableVisualStyles()\n$out='{}'\n\
             $owner=New-Object System.Windows.Forms.Form\n$owner.TopMost=$true\n$owner.ShowInTaskbar=$false\n\
             $owner.StartPosition='CenterScreen'\n$owner.Size=New-Object System.Drawing.Size(1,1)\n$owner.Opacity=0\n$owner.Show()\n\
             $dlg=New-Object System.Windows.Forms.OpenFileDialog\n$dlg.Title='sluiceULPmax - choose files'\n$dlg.Multiselect=$true\n\
             $dlg.Filter='Text (*.txt)|*.txt|All files (*.*)|*.*'\n$res='CANCEL'\n\
             if ($dlg.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) {{ $res=($dlg.FileNames -join ([char]10)) }}\n\
             $dlg.Dispose()\n$owner.Close()\n[System.IO.File]::WriteAllText($out,$res,[System.Text.Encoding]::UTF8)\n",
            out_path.display().to_string().replace('\\', "\\\\")
        )
    };
    let _ = init;
    if fs::write(&ps_path, script).is_err() {
        return None;
    }
    screen.off();
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&ps_path)
        .status();
    screen.on();
    let _ = fs::remove_file(&ps_path);
    if status.is_err() {
        return None;
    }
    let txt = fs::read_to_string(&out_path).ok()?;
    let _ = fs::remove_file(&out_path);
    let t = txt.trim_start_matches('\u{feff}').trim().to_string();
    if t == "CANCEL" {
        return Some(String::new());
    }
    Some(t)
}

fn write_results(run: &Run, out: &crate::ScanOut) -> io::Result<usize> {
    fs::create_dir_all(&run.out_dir)?;
    let mut files: Vec<Option<fs::File>> = (0..run.names.len()).map(|_| None).collect();
    let mut written = 0usize;
    for (p, line) in &out.hits {
        let idx = *p as usize;
        if files[idx].is_none() {
            let name = sanitize(&run.names[idx]);
            files[idx] = Some(fs::File::create(Path::new(&run.out_dir).join(format!("{name}.txt")))?);
            written += 1;
        }
        let mut buf: Vec<u8> = Vec::new();
        match run.format {
            Format::Login => {
                if let Some((_, login, pass)) = parse_cred(line) {
                    buf.extend_from_slice(login);
                    buf.push(b':');
                    buf.extend_from_slice(pass);
                } else {
                    buf.extend_from_slice(strip_cr(line));
                }
            }
            Format::Full => buf.extend_from_slice(strip_cr(line)),
        }
        buf.push(b'\n');
        files[idx].as_mut().unwrap().write_all(&buf)?;
    }
    Ok(written)
}

// ─────────────────────────────────────────────────────────── экраны

fn screen_for(app: &mut App, w: usize) -> Vec<String> {
    if app.modal.is_some() {
        return screen_browser(app, w, h_of());
    }
    let mut v = header(w, app.step);
    match app.step {
        0 => {
            v.push(format!("  {}источники{} {}— файлы и папки, по которым пройдёт один проход{}", BOLD, RESET, DIM, RESET));
            v.push(String::new());
            if app.paths.is_empty() {
                v.push(format!("  {}пусто — добавь файл или папку{}", DIM, RESET));
            } else {
                for (i, p) in app.paths.iter().enumerate() {
                    let mark = if i == app.sel { "▸" } else { " " };
                    let kind = if p.is_dir { "папка" } else { "файл" };
                    app.rows.push((v.len() as u16, i));
                    v.push(format!(
                        "  {} {:>2}  {:>10}  {}  {}",
                        mark, i + 1, human(p.bytes as f64), fit(&p.root, w.saturating_sub(40)), kind
                    ));
                }
                v.push(String::new());
                v.push(format!("  {}итого{} {} файлов · {}", DIM, RESET, app.total_files(), human(app.total_bytes() as f64)));
            }
            v.push(String::new());
            v.push(String::new());
            let list = [
                Btn { label: "+ файл".into(), action: "addFile" },
                Btn { label: "+ папка".into(), action: "addFolder" },
                Btn { label: "убрать".into(), action: "remove" },
                Btn { label: "далее".into(), action: "next" },
            ];
            let y = v.len();
            v.push(draw_buttons(&list, app.focus.min(3), y, w, app));
        }
        1 => {
            let qs = app.queries();
            v.push(format!("  {}запросы{} {}— через | или с новой строки, регистр не важен{}", BOLD, RESET, DIM, RESET));
            v.push(String::new());
            let box_w = w.saturating_sub(6).min(96).max(30);
            let inner = box_w - 4;
            let cur = app.qcur.min(app.query.chars().count());
            let chars: Vec<char> = app.query.chars().collect();
            let mut marked = String::new();
            for (i, c) in chars.iter().enumerate() {
                if i == cur {
                    marked.push('\u{0}');
                }
                marked.push(*c);
            }
            if cur >= chars.len() {
                marked.push('\u{0}');
            }
            let mut chunks: Vec<String> = Vec::new();
            let mut tmp = String::new();
            for c in marked.chars() {
                if tmp.chars().count() >= inner {
                    chunks.push(tmp.clone());
                    tmp.clear();
                }
                tmp.push(c);
            }
            chunks.push(tmp);
            v.push(format!("  {}┌{}┐{}", DIM, "─".repeat(box_w - 2), RESET));
            for ch in chunks.iter().take(6) {
                let disp = ch.replace('\u{0}', &format!("{}▏{}", CYAN, RESET)).replace('\n', "⏎");
                v.push(format!("  {}│{} {} {}│{}", DIM, RESET, pad_vis(&disp, inner), DIM, RESET));
            }
            v.push(format!("  {}└{}┘{}", DIM, "─".repeat(box_w - 2), RESET));
            v.push(String::new());
            v.push(format!(
                "  {}распознано{} {} {} · символов {}",
                DIM, RESET, qs.len(), plural(qs.len(), "запрос", "запроса", "запросов"), app.query.chars().count()
            ));
            v.push(String::new());
            let list = [
                Btn { label: "из файла".into(), action: "queriesFile" },
                Btn { label: "очистить".into(), action: "clear" },
                Btn { label: "назад".into(), action: "back" },
                Btn { label: "далее".into(), action: "next" },
            ];
            let y = v.len();
            v.push(draw_buttons(&list, 9, y, w, app));
        }
        2 => {
            let qs = app.queries();
            v.push(format!("  {}вывод{} {}— в каком виде писать найденные строки{}", BOLD, RESET, DIM, RESET));
            v.push(String::new());
            v.push(opt_line(app.format == Format::Full, "url:login:pass", "— как в дампе, полная строка"));
            v.push(opt_line(app.format == Format::Login, "login:pass", "— только учётка, без url"));
            v.push(opt_line(app.count_only, "только счётчики", "— строки не пишутся"));
            v.push(String::new());
            v.push(format!("  {}папка вывода{}  {}{}{}", DIM, RESET, WHITE, app.out_dir, RESET));
            let abs = std::env::current_dir()
                .map(|d| d.join(&app.out_dir).display().to_string())
                .unwrap_or_else(|_| app.out_dir.clone());
            v.push(format!("               {}{}{}", DIM, abs, RESET));
            v.push(String::new());
            v.push(format!(
                "  {}к запуску:{} {} источников · {} · {} запросов",
                DIM, RESET, app.paths.len(), human(app.total_bytes() as f64), qs.len()
            ));
            v.push(String::new());
            let list = [
                Btn { label: "формат".into(), action: "toggleFormat" },
                Btn { label: "счётчики".into(), action: "toggleCount" },
                Btn { label: "папка".into(), action: "pickDir" },
                Btn { label: "назад".into(), action: "back" },
                Btn { label: "запустить".into(), action: "run" },
            ];
            let y = v.len();
            v.push(draw_buttons(&list, app.focus.min(4), y, w, app));
        }
        3 => {
            let (bytes, hits) = app
                .run
                .as_ref()
                .map(|r| r.prog.snapshot())
                .unwrap_or((0, 0));
            let total = app.run.as_ref().map(|r| r.prog.total).unwrap_or(1).max(1);
            let pct = (bytes as f64 / total as f64 * 100.0).min(100.0);
            let secs = app.run.as_ref().map(|r| r.started.elapsed().as_secs_f64()).unwrap_or(0.0).max(0.001);
            let speed = bytes as f64 / secs / 1e6;
            let bar_w = w.saturating_sub(24).clamp(20, 64);
            v.push(String::new());
            v.push(center(&format!("{}{:>3}{}%", BOLD, pct as u32, RESET), w));
            let filled = (pct / 100.0 * bar_w as f64) as usize;
            let bar: String = (0..bar_w)
                .map(|i| if i < filled { '█' } else { '░' })
                .collect();
            v.push(center(&format!("{}{}{}", CYAN, bar, RESET), w));
            v.push(String::new());
            v.push(center(
                &format!(
                    "{} / {}  ·  {:.0} МБ/с  ·  найдено {}",
                    human(bytes as f64), human(total as f64), speed, hits
                ),
                w,
            ));
            v.push(String::new());
            let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let t = secs as usize / 7 % spin.len();
            v.push(center(&format!("{} работаю{}", spin[t], if no_color() { "" } else { "" }), w));
            // живые лидеры
            if let Some(run) = &app.run {
                let mut top: Vec<(usize, u64)> = (0..run.names.len())
                    .map(|i| (i, run.prog.counts[i].load(Ordering::Relaxed)))
                    .filter(|(_, c)| *c > 0)
                    .collect();
                top.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
                v.push(String::new());
                for (i, c) in top.into_iter().take(3) {
                    v.push(center(&format!("{:<28} {:>8}", run.names[i], c), w));
                }
            }
            v.push(String::new());
            v.push(String::new());
            v.push(center(&format!("{}Esc — прервать{}", DIM, RESET), w));
        }
        _ => {
            if let Some(r) = &app.res {
                let matched = r.counts.iter().filter(|c| **c > 0).count();
                v.push(format!(
                    "  ✔ {}готово{} · {:.1} s · {:.0} МБ/с · {} · {} совпадений",
                    GREEN, RESET, r.secs, r.bytes as f64 / r.secs.max(0.001) / 1e6, human(r.bytes as f64), r.hits
                ));
                v.push(format!("  {}{}{}", DIM, "─".repeat(w.saturating_sub(3)), RESET));
                let mut order: Vec<usize> = (0..r.counts.len()).collect();
                order.sort_by_key(|&i| std::cmp::Reverse(r.counts[i]));
                for &i in order.iter().take(8) {
                    if r.counts[i] > 0 {
                        v.push(format!("  {:<48} {:>9}", r.patterns[i], r.counts[i]));
                    }
                }
                v.push(format!("  {}{}{}", DIM, "─".repeat(w.saturating_sub(3)), RESET));
                v.push(format!(
                    "  сработали {}/{} · записано в {}",
                    matched, r.counts.len(), r.out_dir
                ));
            }
            v.push(String::new());
            let list = [
                Btn { label: "ещё раз".into(), action: "again" },
                Btn { label: "выход".into(), action: "quit" },
            ];
            let y = v.len();
            v.push(draw_buttons(&list, app.focus.min(1), y, w, app));
        }
    }
    if !app.msg.is_empty() {
        v.push(String::new());
        v.push(format!("  {}{}{}", DIM, app.msg, RESET));
    }
    let hint = match app.step {
        0 => "↑↓ выбор · Tab кнопки · Enter нажать · Del убрать · Q выход",
        1 => "печатай или вставляй · Ctrl+V вставить · Enter далее · Q выход",
        2 => "Tab кнопки · Enter нажать · F формат · C счётчики · P папка · Esc назад",
        3 => "Esc — прервать",
        _ => "Enter — заново · Q — выход",
    };
    v.extend(footer(w, hint));
    v
}

fn h_of() -> usize {
    term_size().1
}

fn opt_line(active: bool, title: &str, desc: &str) -> String {
    if active {
        format!("  {}●{} {}{}{} {}{}{}", GREEN, RESET, WHITE, title, RESET, DIM, desc, RESET)
    } else {
        format!("  {}○{} {}{}{} {}{}{}", DIM, RESET, GRAY, title, RESET, DIM, desc, RESET)
    }
}

fn screen_browser(app: &mut App, w: usize, h: usize) -> Vec<String> {
    let (dir, entries, idx) = {
        let b = app.modal.as_ref().unwrap();
        (b.dir.clone(), b.entries.clone(), b.idx)
    };
    let mut v: Vec<String> = Vec::new();
    let box_w = w.saturating_sub(6).min(92);
    let box_h = (h.saturating_sub(6)).min(18);
    let pad_top = (h.saturating_sub(box_h) / 2).max(1);
    let pad_left = ((w.saturating_sub(box_w)) / 2).max(2);
    for _ in 0..pad_top {
        v.push(String::new());
    }
    v.push(format!("{} {}выбрать папку{} {}", " ".repeat(pad_left), INV, RESET, DIM));
    v.push(format!("{}{}┌{}┐{}", " ".repeat(pad_left), DIM, "─".repeat(box_w - 2), RESET));
    let list_h = box_h.saturating_sub(4);
    let start = idx.saturating_sub(list_h / 2).min(entries.len().saturating_sub(list_h));
    for i in 0..list_h {
        let e = entries.get(start + i);
        match e {
            None => v.push(format!("{}{}│{}│{}", " ".repeat(pad_left), DIM, " ".repeat(box_w - 2), RESET)),
            Some((name, _p, is_dir, size)) => {
                let active = start + i == idx;
                let label = if *is_dir { format!("{}/", name) } else { name.clone() };
                let sz = if *is_dir { String::new() } else { human(*size as f64) };
                let mark = if active { "▸" } else { " " };
                v.push(format!(
                    "{}{}│{} {} {:>9} {}│{}",
                    " ".repeat(pad_left), DIM, RESET, pad_vis(&format!("{} {}", mark, label), box_w - 16), sz, DIM, RESET
                ));
            }
        }
    }
    v.push(format!("{}{}└{}┘{}", " ".repeat(pad_left), DIM, "─".repeat(box_w - 2), RESET));
    v.push(String::new());
    v.push(format!(
        "{}{}↑↓ выбор · Enter войти/добавить · Esc отмена{}",
        " ".repeat(pad_left), DIM, RESET
    ));
    let _ = dir;
    v
}

// ─────────────────────────────────────────────────────────── действия браузера

fn open_browser(app: &mut App, dir: PathBuf, target: Target) {
    match fs::read_dir(&dir) {
        Ok(rd) => {
            let mut entries: Vec<(String, PathBuf, bool, u64)> = rd
                .flatten()
                .map(|e| {
                    let p = e.path();
                    let is_dir = p.is_dir();
                    let size = if is_dir { 0 } else { fs::metadata(&p).map(|m| m.len()).unwrap_or(0) };
                    (e.file_name().to_string_lossy().to_string(), p, is_dir, size)
                })
                .collect();
            entries.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
            if let Some(parent) = dir.parent() {
                entries.insert(0, ("..".into(), parent.to_path_buf(), true, 0));
            }
            app.last_dir = dir.clone();
            app.modal = Some(Browser { dir, entries, idx: 0, target });
        }
        Err(_) => app.msg = format!("не открыть: {}", dir.display()),
    }
}

fn browser_key(app: &mut App, ev: Ev) {
    let (up, down, enter, space) = match ev {
        Ev::Up => (true, false, false, false),
        Ev::Down => (false, true, false, false),
        Ev::Enter => (false, false, true, false),
        Ev::Char(' ') => (false, false, false, true),
        Ev::Esc => {
            app.modal = None;
            return;
        }
        _ => (false, false, false, false),
    };
    let b = app.modal.as_mut().unwrap();
    if up {
        b.idx = b.idx.saturating_sub(1);
    }
    if down {
        b.idx = (b.idx + 1).min(b.entries.len().saturating_sub(1));
    }
    let target = b.target;
    let (name, path, is_dir, _) = b.entries[b.idx].clone();
    if enter {
        if is_dir {
            open_browser(app, path.clone(), target);
        } else {
            match target {
                Target::Queries => match fs::read_to_string(&path) {
                    Ok(t) => {
                        app.query = t.trim().to_string();
                        app.qcur = app.query.chars().count();
                        app.msg = format!("запросы из {}", name);
                    }
                    Err(_) => app.msg = "не смог прочитать файл".into(),
                },
                _ => app.add_path(&path.display().to_string()),
            }
            app.modal = None;
        }
    }
    if space && is_dir {
        if target == Target::OutDir {
            app.out_dir = path.display().to_string();
            app.msg = format!("папка вывода: {}", app.out_dir);
        } else {
            app.add_path(&path.display().to_string());
        }
        app.modal = None;
    }
}

fn paste_clipboard(app: &mut App, _screen: &mut Screen) -> io::Result<()> {
    let text = read_clipboard();
    if text.is_empty() {
        app.msg = "буфер обмена пуст".into();
        return Ok(());
    }
    if app.step == 1 {
        let cur = app.qcur.min(app.query.chars().count());
        let mut chars: Vec<char> = app.query.chars().collect();
        for (k, c) in text.chars().enumerate() {
            chars.insert(cur + k, c);
        }
        app.query = chars.into_iter().collect();
        app.qcur = cur + text.chars().count();
        app.msg = format!("вставлено {} символов", text.chars().count());
    } else if app.step == 0 {
        let mut n = 0;
        for line in text.split('\n') {
            let p = line.trim().trim_matches('"');
            if !p.is_empty() {
                app.add_path(p);
                n += 1;
            }
        }
        app.msg = format!("добавлено путей: {}", n);
    }
    Ok(())
}

fn read_clipboard() -> String {
    let tmp = std::env::temp_dir().join(format!("sluiceulp-clip-{}.txt", std::process::id()));
    let script = format!(
        "$t = Get-Clipboard -Raw; if ($null -eq $t) {{ $t = '' }}; [System.IO.File]::WriteAllText('{}', [string]$t, [System.Text.Encoding]::UTF8)",
        tmp.display()
    );
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let out = fs::read_to_string(&tmp).unwrap_or_default();
    let _ = fs::remove_file(&tmp);
    out.trim_start_matches('\u{feff}').to_string()
}
