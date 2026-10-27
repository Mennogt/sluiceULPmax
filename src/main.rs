// sluiceULP (Rust) — один проход по дампам, любое число литералов.
//
// Два режима:
//   sluiceULPmax                     — интерактивный интерфейс (как в Node-версии)
//   sluiceULPmax <путь> "q1|q2" ...  — командная строка для скриптов
//
// Скорость: файл режется на N блоков по границам строк, каждый поток читает свой
// диапазон через pread, поиск идёт фильтром по парам байт (таблица 64 КиБ в L2).

mod filter;
mod tui;

use std::collections::HashSet;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_corasick::{AhoCorasick, MatchKind};
use filter::Filter;
use memchr::{memchr, memrchr};

#[cfg(windows)]
use std::os::windows::fs::{FileExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::fs::FileExt;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn SetConsoleOutputCP(code: u32) -> i32;
}

pub const VERSION: &str = "1.0.0";

// ─────────────────────────────────────────────────────────── чтение по смещению

#[cfg(windows)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    f.seek_read(buf, off)
}
#[cfg(unix)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    f.read_at(buf, off)
}

pub fn read_full(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut got = 0usize;
    while got < buf.len() {
        match pread(f, &mut buf[got..], off + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

#[cfg(windows)]
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

pub fn open_sequential(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    o.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    o.open(path)
}

// ─────────────────────────────────────────────────────────── границы блоков

fn align_forward(f: &File, len: u64, from: u64) -> io::Result<u64> {
    if from == 0 || from >= len {
        return Ok(from.min(len));
    }
    let mut pos = from;
    let mut buf = vec![0u8; 1 << 16];
    while pos < len {
        let n = read_full(f, &mut buf, pos)?;
        if n == 0 {
            return Ok(len);
        }
        if let Some(i) = memchr(b'\n', &buf[..n]) {
            return Ok(pos + i as u64 + 1);
        }
        pos += n as u64;
    }
    Ok(len)
}

// ─────────────────────────────────────────────────────────── движок

pub enum Kind {
    Filter(Filter),
    Ac(AhoCorasick),
}

pub struct Engine {
    kind: Kind,
    ci: bool,
}

impl Engine {
    pub fn build(patterns: &[String], ci: bool, name: &str) -> Engine {
        let kind = match name {
            "ac" => Kind::Ac(
                AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .ascii_case_insensitive(ci)
                    .build(patterns)
                    .expect("автомат"),
            ),
            _ => Kind::Filter(Filter::build(patterns, ci)),
        };
        Engine { kind, ci }
    }

    pub fn max_len(&self) -> usize {
        match &self.kind {
            Kind::Filter(f) => f.max_len(),
            Kind::Ac(a) => a.max_pattern_len(),
        }
    }

    pub fn npat(&self) -> usize {
        match &self.kind {
            Kind::Filter(f) => f.patterns().len(),
            Kind::Ac(a) => a.patterns_len(),
        }
    }

    #[inline]
    pub fn scan_region(&self, region: &[u8], counts: &mut [u64], on_hit: &mut dyn FnMut(usize, usize)) {
        match &self.kind {
            Kind::Filter(f) => {
                let ci = self.ci;
                f.scan(region, ci, counts, |pi, start, _end| on_hit(pi, start));
            }
            Kind::Ac(a) => {
                for m in a.find_iter(region) {
                    let p = m.pattern().as_usize();
                    counts[p] += 1;
                    on_hit(p, m.start());
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────── прогресс

pub struct Prog {
    pub bytes: AtomicU64,
    pub hits: AtomicU64,
    pub counts: Vec<AtomicU64>,
    pub total: u64,
    pub stop: AtomicBool,
}

impl Prog {
    pub fn new(npat: usize, total: u64) -> Prog {
        Prog {
            bytes: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            counts: (0..npat).map(|_| AtomicU64::new(0)).collect(),
            total,
            stop: AtomicBool::new(false),
        }
    }
    pub fn snapshot(&self) -> (u64, u64) {
        (self.bytes.load(Ordering::Relaxed), self.hits.load(Ordering::Relaxed))
    }
}

// ─────────────────────────────────────────────────────────── поток

pub struct WorkerResult {
    pub hits: Vec<(u32, Vec<u8>)>,
    pub bytes: u64,
}

pub fn worker(
    eng: Arc<Engine>,
    file: Arc<File>,
    prog: Arc<Prog>,
    start: u64,
    end: u64,
    chunk: usize,
    want_lines: bool,
    overlap: usize,
) -> io::Result<WorkerResult> {
    let mut local = vec![0u64; eng.npat()];
    let mut hits: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut buf = vec![0u8; chunk + overlap + 1];
    let mut tail = 0usize;
    let mut pos = start;
    let mut bytes = 0u64;

    loop {
        if prog.stop.load(Ordering::Relaxed) {
            break;
        }
        let room = buf.len() - tail;
        let want = ((end.saturating_sub(pos)) as usize).min(chunk).min(room);
        let n = if want > 0 {
            read_full(&file, &mut buf[tail..tail + want], pos)?
        } else {
            0
        };
        pos += n as u64;
        let total = tail + n;
        if total == 0 {
            break;
        }

        let at_end = pos >= end;
        let cut = if at_end {
            total
        } else {
            match memrchr(b'\n', &buf[..total]) {
                Some(i) => i + 1,
                None => total.saturating_sub(overlap),
            }
        };

        let region = &buf[..cut];
        for c in local.iter_mut() {
            *c = 0;
        }
        {
            let mut collect = |pi: usize, start: usize| {
                if want_lines {
                    let ls = memrchr(b'\n', &region[..start]).map(|i| i + 1).unwrap_or(0);
                    let le = memchr(b'\n', &region[start..])
                        .map(|i| start + i)
                        .unwrap_or(region.len());
                    hits.push((pi as u32, region[ls..le].to_vec()));
                }
            };
            eng.scan_region(region, &mut local, &mut collect);
        }
        bytes += cut as u64;
        prog.bytes.fetch_add(cut as u64, Ordering::Relaxed);

        let mut new_hits = 0u64;
        for (i, c) in local.iter().enumerate() {
            if *c > 0 {
                prog.counts[i].fetch_add(*c, Ordering::Relaxed);
                new_hits += *c;
            }
        }
        prog.hits.fetch_add(new_hits, Ordering::Relaxed);

        if at_end {
            break;
        }
        buf.copy_within(cut..total, 0);
        tail = total - cut;
    }

    Ok(WorkerResult { hits, bytes })
}

// ─────────────────────────────────────────────────────────── запуск скана

pub struct ScanPlan {
    pub file: Arc<File>,
    pub len: u64,
    pub bounds: Vec<u64>,
    pub chunk: usize,
    pub threads: usize,
    pub overlap: usize,
    pub want_lines: bool,
}

impl ScanPlan {
    pub fn build(file: Arc<File>, len: u64, threads: usize, chunk_mb: usize, overlap: usize, want_lines: bool) -> io::Result<ScanPlan> {
        let chunk = chunk_mb.max(1) * 1024 * 1024;
        let threads = threads.max(1);
        let mut bounds: Vec<u64> = Vec::with_capacity(threads + 1);
        bounds.push(0);
        for t in 1..threads {
            let nominal = len * t as u64 / threads as u64;
            bounds.push(align_forward(&file, len, nominal)?);
        }
        bounds.push(len);
        bounds.dedup();
        let real = bounds.len() - 1;
        Ok(ScanPlan { file, len, bounds, chunk, threads: real, overlap, want_lines })
    }
}

pub struct ScanOut {
    pub counts: Vec<u64>,
    pub hits: Vec<(u32, Vec<u8>)>,
    pub bytes: u64,
    pub secs: f64,
}

pub fn start_scan(eng: Arc<Engine>, plan: ScanPlan, prog: Arc<Prog>) -> std::thread::JoinHandle<io::Result<ScanOut>> {
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut handles = Vec::new();
        for t in 0..plan.threads {
            let eng = Arc::clone(&eng);
            let f = Arc::clone(&plan.file);
            let pr = Arc::clone(&prog);
            let lo = plan.bounds[t];
            let hi = plan.bounds[t + 1];
            let chunk = plan.chunk;
            let want_lines = plan.want_lines;
            let overlap = plan.overlap;
            handles.push(std::thread::spawn(move || worker(eng, f, pr, lo, hi, chunk, want_lines, overlap)));
        }
        let mut hits = Vec::new();
        let mut bytes = 0u64;
        for h in handles {
            let r = h.join().unwrap()?;
            bytes += r.bytes;
            hits.extend(r.hits);
        }
        let counts: Vec<u64> = prog.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        Ok(ScanOut { counts, hits, bytes, secs: t0.elapsed().as_secs_f64() })
    })
}

// ─────────────────────────────────────────────────────────── разбор строки

pub fn strip_cr(line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\r') {
        &line[..line.len() - 1]
    } else {
        line
    }
}

pub fn parse_cred(line: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let line = strip_cr(line);
    let mut idx: Vec<usize> = memchr::memchr_iter(b':', line).collect();
    if idx.len() < 2 {
        return None;
    }
    let p2 = idx.pop().unwrap();
    let p1 = idx.pop().unwrap();
    let mut url = &line[..p1];
    let login = &line[p1 + 1..p2];
    let pass = &line[p2 + 1..];
    if login.is_empty() {
        return None;
    }
    if url.len() > 3 && url[2] == b' ' && url[0].is_ascii_uppercase() && url[1].is_ascii_uppercase() {
        url = &url[3..];
    }
    Some((url, login, pass))
}

// ─────────────────────────────────────────────────────────── бенчи

fn bench_io(file: Arc<File>, len: u64, threads: usize, chunk: usize) -> (u64, f64) {
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for t in 0..threads {
        let f = Arc::clone(&file);
        let lo = len * t as u64 / threads as u64;
        let hi = len * (t + 1) as u64 / threads as u64;
        handles.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; chunk];
            let mut pos = lo;
            let mut got = 0u64;
            while pos < hi {
                let want = ((hi - pos) as usize).min(chunk);
                match read_full(&f, &mut buf[..want], pos) {
                    Ok(0) => break,
                    Ok(n) => {
                        got += n as u64;
                        pos += n as u64;
                    }
                    Err(_) => break,
                }
            }
            got
        }));
    }
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    (total, t0.elapsed().as_secs_f64())
}

// ─────────────────────────────────────────────────────────── подбор потоков

pub fn probe_speeds(file: &File, len: u64, eng: &Engine, mb: usize) -> (f64, f64) {
    let probe = (mb * 1024 * 1024).min(len as usize).max(1);
    let mut buf = vec![0u8; probe];
    let t = Instant::now();
    let n = read_full(file, &mut buf, 0).unwrap_or(0);
    let disk = if n > 0 { n as f64 / t.elapsed().as_secs_f64().max(1e-9) / 1e6 } else { 0.0 };
    let mut counts = vec![0u64; eng.npat()];
    let t2 = Instant::now();
    let mut sink = |_p: usize, _s: usize| {};
    eng.scan_region(&buf[..n], &mut counts, &mut sink);
    let scan = if n > 0 { n as f64 / t2.elapsed().as_secs_f64().max(1e-9) / 1e6 } else { 0.0 };
    (disk, scan)
}

pub fn recommend(disk: f64, scan: f64, cores: usize) -> usize {
    let cores = cores.max(2);
    if scan <= 1.0 || disk <= 1.0 {
        return cores;
    }
    let need_cold = (disk / (scan * 0.8)).ceil() as usize;
    if disk < 250.0 {
        need_cold.clamp(2, cores)
    } else {
        cores
    }
}

pub fn human(n: f64) -> String {
    // как в проводнике Windows: 1 КБ = 1024 байта, а не 1000
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n;
    let mut u = 0usize;
    while v >= 1024.0 && u < 3 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", n as u64)
    } else if v < 10.0 {
        format!("{:.2} {}", v, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

pub fn sanitize(p: &str) -> String {
    let mut s: String = p
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    s = s.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() {
        s = "query".into();
    }
    s.truncate(72);
    s
}

// ─────────────────────────────────────────────────────────── main

fn main() -> io::Result<()> {
    #[cfg(windows)]
    unsafe {
        SetConsoleOutputCP(65001);
    }

    let args: Vec<String> = env::args().skip(1).collect();

    // без аргументов и в терминале — интерфейс
    let force_tui = std::env::var("SLUICE_FORCE_TUI").map(|v| v == "1").unwrap_or(false);
    if force_tui || (args.is_empty() && io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return tui::run();
    }

    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("sluiceULPmax {} — один проход по дампам, любое число литералов\n", VERSION);
        println!("  sluiceULPmax                     интерактивный интерфейс");
        println!("  sluiceULPmax <файл|папка> \"q1|q2\" [ключи]");
        println!("  sluiceULPmax <файл> --file списки.txt [ключи]");
        println!("  sluiceULPmax <файл> --caps [--file списки.txt]\n");
        println!("  --count              только посчитать, строки не писать");
        println!("  --tsv                вывод count<TAB>pattern, для сверок");
        println!("  --out <папка>        куда писать (по умолчанию hits)");
        println!("  --format url|login|raw   вид выходной строки");
        println!("  --threads auto|N     потоков (по умолчанию auto: замер и подбор)");
        println!("  --chunk <МБ>         буфер на поток (по умолчанию 32)");
        println!("  --engine filter|ac   движок поиска (по умолчанию filter)");
        println!("  --caps               диагностика: ядра, диск, рекомендация по потокам");
        println!("  --bench io|cpu:N|scan    замеры: диск / процессор / полный проход");
        println!("  --no-ci              учитывать регистр");
        println!("  -V, --version        версия");
        return Ok(());
    }

    let mut path: Option<String> = None;
    let mut pats_raw: Option<String> = None;
    let mut file_list: Option<String> = None;
    let mut count_only = false;
    let mut tsv = false;
    let mut out_dir = String::from("hits");
    let mut format = String::from("url");
    let mut threads_raw = String::from("auto");
    let mut chunk_mb = 32usize;
    let mut bench: Option<String> = None;
    let mut engine_name = String::from("filter");
    let mut no_ci = false;
    let mut caps = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--count" => count_only = true,
            "--tsv" => tsv = true,
            "--no-ci" => no_ci = true,
            "--caps" => caps = true,
            "--version" | "-V" => {
                println!("sluiceULPmax {}", VERSION);
                return Ok(());
            }
            "--threads" | "-T" => {
                i += 1;
                threads_raw = args[i].clone();
            }
            "--out" => {
                i += 1;
                out_dir = args[i].clone();
            }
            "--format" => {
                i += 1;
                format = args[i].clone();
            }
            "--chunk" => {
                i += 1;
                chunk_mb = args[i].parse().unwrap_or(chunk_mb);
            }
            "--engine" => {
                i += 1;
                engine_name = args[i].clone();
            }
            "--file" => {
                i += 1;
                file_list = Some(args[i].clone());
            }
            "--bench" => {
                i += 1;
                bench = Some(args[i].clone());
            }
            other => {
                if path.is_none() {
                    path = Some(other.to_string());
                } else if pats_raw.is_none() {
                    pats_raw = Some(other.to_string());
                }
            }
        }
        i += 1;
    }

    let path = path.expect("нужен путь");
    let mut patterns: Vec<String> = Vec::new();
    if let Some(f) = &file_list {
        let text = std::fs::read_to_string(f)?;
        patterns.extend(
            text.split(|c| c == '\n' || c == '\r' || c == '|')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    if let Some(p) = &pats_raw {
        patterns.extend(p.split('|').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    let mut seen = HashSet::new();
    patterns.retain(|p| seen.insert(p.clone()));
    if caps && patterns.is_empty() {
        patterns = [
            "skins.example", "market.example", "trading.example", "exchange.example",
            "wallet.example", "auction.example", "shop.example", "flips.example",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
    }
    if patterns.is_empty() {
        eprintln!("нет запросов");
        std::process::exit(2);
    }

    let file = Arc::new(open_sequential(Path::new(&path))?);
    let len = file.metadata()?.len();
    let ci = !no_ci;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let requested: Option<usize> = match threads_raw.as_str() {
        "auto" | "0" | "" => None,
        v => v.parse::<usize>().ok().map(|n| n.max(1)),
    };

    if bench.as_deref() == Some("io") {
        let threads = requested.unwrap_or(cores);
        let (got, secs) = bench_io(Arc::clone(&file), len, threads, chunk_mb.max(1) * 1024 * 1024);
        println!(
            "io only:   {} за {:.2} с = {:.0} МБ/с  (потоков {}, буфер {} МБ)",
            human(got as f64), secs, got as f64 / secs / 1e6, threads, chunk_mb
        );
        return Ok(());
    }

    let engine = Arc::new(Engine::build(&patterns, ci, &engine_name));

    if let Some(spec) = bench.as_deref().filter(|s| s.starts_with("cpu")) {
        let mb: usize = spec.strip_prefix("cpu:").and_then(|v| v.parse().ok()).unwrap_or(256);
        let take = (mb * 1024 * 1024).min(len as usize);
        let mut buf = vec![0u8; take];
        read_full(&file, &mut buf, 0)?;
        let mut counts = vec![0u64; patterns.len()];
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            let mut sink = |_p: usize, _s: usize| {};
            engine.scan_region(&buf, &mut counts, &mut sink);
            best = best.min(t.elapsed().as_secs_f64());
        }
        let total: u64 = counts.iter().sum();
        println!(
            "cpu only:  {} МБ, {} шаблонов, движок {} → {:.0} МБ/с в одном потоке (совпадений {})",
            take / 1024 / 1024, patterns.len(), engine_name, take as f64 / best / 1e6, total
        );
        return Ok(());
    }

    let want_lines = !count_only && !tsv;
    let overlap = engine.max_len().max(1);

    let need_probe = caps || requested.is_none();
    let (disk, scan) = if need_probe && len > 64 * 1024 * 1024 {
        probe_speeds(&file, len, &engine, 32)
    } else {
        (0.0, 0.0)
    };
    let rec = recommend(disk, scan, cores);

    if caps {
        println!("sluiceULPmax {} — диагностика", VERSION);
        println!("  логических ядер   {}", cores);
        println!("  файл              {}", human(len as f64));
        println!("  чтение (замер)    {:.0} МБ/с", disk);
        println!("  сканер, 1 поток   {:.0} МБ/с (на {} запросах)", scan, patterns.len());
        println!(
            "  нужно потоков     {}",
            if scan > 1.0 && disk > 1.0 { format!("{:.0}", (disk / (scan * 0.8)).ceil()) } else { "—".into() }
        );
        println!("  рекомендуется     {}", rec);
        println!("  задано            {}", match requested { Some(n) => n.to_string(), None => "авто".into() });
        return Ok(());
    }

    let (threads, threads_mode) = match requested {
        Some(n) => (n, "задано"),
        None => (rec, "авто"),
    };
    let threads = threads.max(1).min(((len / (8 * 1024 * 1024)) as usize).max(1));

    let plan = ScanPlan::build(Arc::clone(&file), len, threads, chunk_mb, overlap, want_lines)?;
    let prog = Arc::new(Prog::new(engine.npat(), len));
    let show_progress = io::stderr().is_terminal();
    let real_threads = plan.threads;

    let reporter = if show_progress {
        let p = Arc::clone(&prog);
        Some(std::thread::spawn(move || {
            let t_start = Instant::now();
            while !p.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                let (b, h) = p.snapshot();
                let pct = if p.total > 0 { b as f64 * 100.0 / p.total as f64 } else { 100.0 };
                let sp = b as f64 / t_start.elapsed().as_secs_f64().max(1e-9) / 1e6;
                eprint!(
                    "\r  {:>5.1}%  {} / {}  {:.0} МБ/с  совпадений {}   ",
                    pct, human(b as f64), human(p.total as f64), sp, h
                );
            }
        }))
    } else {
        None
    };

    let handle = start_scan(Arc::clone(&engine), plan, Arc::clone(&prog));
    let out = handle.join().unwrap()?;
    prog.stop.store(true, Ordering::Relaxed);
    if let Some(h) = reporter {
        let _ = h.join();
        eprint!("\r{}\r", " ".repeat(78));
    }

    let counts = out.counts;
    let total: u64 = counts.iter().sum();
    let secs = out.secs;
    let bytes = out.bytes;

    if bench.as_deref() == Some("scan") {
        println!(
            "scan only: {} за {:.2} с = {:.0} МБ/с, совпадений {}, потоков {}, движок {}",
            human(bytes as f64), secs, bytes as f64 / secs / 1e6, total, real_threads, engine_name
        );
        return Ok(());
    }

    if !count_only && !tsv {
        std::fs::create_dir_all(&out_dir)?;
        let mut files: Vec<Option<File>> = (0..patterns.len()).map(|_| None).collect();
        for (p, line) in &out.hits {
            let idx = *p as usize;
            if files[idx].is_none() {
                let name = sanitize(&patterns[idx]);
                files[idx] = Some(File::create(Path::new(&out_dir).join(format!("{name}.txt")))?);
            }
            let mut buf: Vec<u8> = Vec::with_capacity(line.len() + 1);
            match format.as_str() {
                "login" => {
                    if let Some((_, login, pass)) = parse_cred(line) {
                        buf.extend_from_slice(login);
                        buf.push(b':');
                        buf.extend_from_slice(pass);
                    } else {
                        buf.extend_from_slice(strip_cr(line));
                    }
                }
                _ => buf.extend_from_slice(strip_cr(line)),
            }
            buf.push(b'\n');
            files[idx].as_mut().unwrap().write_all(&buf)?;
        }
    }

    if tsv {
        for (i, p) in patterns.iter().enumerate() {
            println!("{}\t{}", counts[i], p);
        }
        return Ok(());
    }

    let mut order: Vec<usize> = (0..counts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
    println!(
        "готово:    {} за {:.2} с = {:.0} МБ/с · {} совпадений · потоков {} ({}) · движок {}",
        human(bytes as f64), secs, bytes as f64 / secs / 1e6, total, real_threads, threads_mode, engine_name
    );
    for &i in order.iter().take(20) {
        if counts[i] > 0 {
            println!("  {:>10}  {}", counts[i], patterns[i]);
        }
    }
    Ok(())
}
