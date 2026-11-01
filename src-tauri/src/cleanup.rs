//! Transcript cleanup between speech recognition and paste
//! (docs/AI-CLEANUP-PLAN.md). "Light" is deterministic rules that run in
//! microseconds: filler words, stutters, spoken line breaks, the user's
//! dictionary and per-app punctuation. Numbers, self-corrections and spoken
//! lists depend on meaning, so they're left for Smart (smart.rs).

use crate::dictionary::Entry;
use crate::learn::{letters, levenshtein};
use regex::{Captures, Regex, RegexBuilder};
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Instant;

pub struct Context {
    /// Lowercased exe of the app being dictated into, e.g. "slack.exe".
    pub app: Option<String>,
    pub light: bool,
    pub dictionary: Vec<Entry>,
    /// Tells real English words from mishearings (`spell::known`). Without
    /// it, only near-misses with exactly the same letters are swapped.
    pub known: Option<KnownWords>,
}

pub type KnownWords = fn(&[&str]) -> Option<Vec<bool>>;

/// Hesitation sounds, including stretched spellings ("uhhh", "ummm", "aah",
/// "ooh"). Plain "oh" and "mhm" carry meaning and are kept.
fn filler_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:u+h+m*|u+m+|e+r+m*|e+h+|a+h+|a{2,}h*|o{2,}h*|o+h{2,}|h+m+|m{2,})$").expect("valid regex")
    })
}
/// After a removed filler, a comma before one of these is kept
/// ("went well, um, and" keeps its comma; "should, uh, ship" loses it).
const CONJUNCTIONS: &[&str] = &["and", "but", "or", "so", "because", "then", "which", "while"];
/// Doubled words that are often correct ("I know that that works").
const REAL_DOUBLES: &[&str] = &["that", "had", "bye", "so", "very"];
const TERMINALS: &[&str] = &[
    "windowsterminal.exe", "cmd.exe", "powershell.exe", "pwsh.exe", "wezterm-gui.exe",
    "alacritty.exe", "conhost.exe", "mintty.exe", "warp.exe", "tabby.exe", "hyper.exe",
];
const CHATS: &[&str] = &[
    "slack.exe", "discord.exe", "whatsapp.exe", "telegram.exe", "ms-teams.exe", "teams.exe",
    "signal.exe", "messenger.exe",
];

pub fn clean(raw: &str, ctx: &Context) -> String {
    if !ctx.light {
        return raw.trim().to_string();
    }
    finish(&light(raw, ctx), ctx)
}

/// The Light rules, before the per-app touches. Smart cleanup runs on this.
pub fn light(raw: &str, ctx: &Context) -> String {
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    let text = spoken_breaks(text);
    let text = text
        .lines()
        .map(|l| collapse_repeats(&collapse_phrases(&remove_fillers(l))))
        .collect::<Vec<_>>()
        .join("\n");
    let text = spoken_quotes(&text);
    let text = key_combos(&text);
    let text = apply_dictionary(&text, &ctx.dictionary);
    text.lines().map(|l| fix_near_misses(l, ctx)).collect::<Vec<_>>().join("\n")
}

/// The last step for both tiers: dictionary spellings (again, in case Smart
/// changed one) and the app's punctuation habits.
pub fn finish(text: &str, ctx: &Context) -> String {
    let text = apply_dictionary(text.trim(), &ctx.dictionary);
    fit_app(text.trim(), ctx.app.as_deref())
}

pub fn is_terminal(exe: &str) -> bool {
    TERMINALS.contains(&exe)
}

pub fn is_chat(exe: &str) -> bool {
    CHATS.contains(&exe)
}

fn core(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric())
}

fn trailing_punct(word: &str) -> &str {
    let end = word.trim_end_matches(|c: char| !c.is_alphanumeric());
    &word[end.len()..]
}

fn ends_sentence(word: &str) -> bool {
    word.ends_with(['.', '!', '?'])
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn lowercase_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn is_filler(word: &str) -> bool {
    let c = core(word);
    // "ER", "UM" written in capitals are acronyms, not hesitations.
    if c.len() > 1 && c.chars().all(|ch| ch.is_ascii_uppercase()) {
        return false;
    }
    filler_regex().is_match(&c.to_lowercase())
}

/// Drops um/uh/hmm and fixes the punctuation around them.
fn remove_fillers(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut capitalize_next = false;
    for (i, w) in words.iter().enumerate() {
        if !is_filler(w) {
            out.push(if capitalize_next { capitalize(w) } else { w.to_string() });
            capitalize_next = false;
            continue;
        }
        let punct = trailing_punct(w);
        let at_sentence_start = out.last().is_none_or(|p| ends_sentence(p));
        if at_sentence_start {
            // "Um, so I think" -> "So I think". Only re-capitalise if the
            // filler itself was capitalised (it started the sentence).
            capitalize_next = capitalize_next || w.starts_with(char::is_uppercase);
            continue;
        }
        let prev = out.last_mut().expect("not at sentence start");
        if punct.contains(['.', '!', '?']) {
            // "ship it, uh." -> "ship it."
            let trimmed = prev.trim_end_matches(',').to_string();
            *prev = trimmed + &punct.replace(',', "");
            continue;
        }
        if prev.ends_with(',') {
            let next = words.get(i + 1);
            if next.is_some_and(|n| is_filler(n)) {
                continue; // decide at the last filler in a run
            }
            let next = next.map(|n| core(n).to_lowercase()).unwrap_or_default();
            if !CONJUNCTIONS.contains(&next.as_str()) {
                prev.pop();
            }
        }
    }
    out.join(" ")
}

/// "I I think the the build" -> "I think the build".
fn collapse_repeats(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for w in line.split_whitespace() {
        if let Some(prev) = out.last_mut() {
            let (a, b) = (core(prev).to_lowercase(), core(w).to_lowercase());
            let prev_bare = trailing_punct(prev).is_empty() && !prev.starts_with(|c: char| !c.is_alphanumeric());
            if prev_bare && a == b && a.chars().all(char::is_alphabetic) && !REAL_DOUBLES.contains(&a.as_str()) {
                prev.push_str(trailing_punct(w));
                continue;
            }
        }
        out.push(w.to_string());
    }
    out.join(" ")
}

/// "you know what, you know what, you know what" -> "you know what".
/// Repeats of 2-6 words in a row are dropped, keeping the last one's
/// punctuation.
fn collapse_phrases(line: &str) -> String {
    let mut words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    for n in (2..=6).rev() {
        let mut i = 0;
        while i + 2 * n <= words.len() {
            let same = (0..n).all(|k| {
                let (a, b) = (core(&words[i + k]).to_lowercase(), core(&words[i + n + k]).to_lowercase());
                !a.is_empty() && a == b
            });
            if same {
                let tail = trailing_punct(&words[i + 2 * n - 1]).to_string();
                let last = &mut words[i + n - 1];
                let bare = last.trim_end_matches(|c: char| !c.is_alphanumeric()).to_string();
                *last = bare + &tail;
                words.drain(i + n..i + 2 * n);
            } else {
                i += 1;
            }
        }
    }
    words.join(" ")
}

fn quotes_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(r"\b(?:open\s+)?quote\b,?\s+(?P<body>.+?)[,.]?\s+(?:end\s+quote|close\s+quote|unquote)\b")
            .case_insensitive(true)
            .build()
            .expect("valid regex")
    })
}

/// "quote hello world end quote" -> "hello world" in quotation marks.
/// Needs both the opening and the closing word, so "quote unquote" and a
/// lone "quote" are left alone.
fn spoken_quotes(text: &str) -> String {
    quotes_regex().replace_all(text, |c: &Captures| format!("\"{}\"", &c["body"])).into_owned()
}

fn combo_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(
            r"\b(?P<mods>(?:(?:control|ctrl|alt|shift|windows|win|command|cmd|option)\s*(?:\+|-|\bplus\b)?\s*)+)(?P<key>f1[0-2]|f[1-9]|page\s+up|page\s+down|print\s+screen|backspace|spacebar|space|enter|return|tab|escape|esc|delete|del|home|end|insert|up|down|left|right|[a-z0-9])\b",
        )
        .case_insensitive(true)
        .build()
        .expect("valid regex")
    })
}

/// "control plus C" -> "Ctrl+C", "alt tab" -> "Alt+Tab". Without a spoken
/// "plus" it only fires for unambiguous keys, so "shift a meeting" and
/// "control panel" stay as words.
fn key_combos(text: &str) -> String {
    combo_regex()
        .replace_all(text, |c: &Captures| {
            let whole = &c[0];
            let mods_text = c["mods"].to_lowercase();
            let explicit = mods_text.contains('+') || mods_text.contains('-') || mods_text.contains("plus");
            let mods: Vec<&str> = mods_text
                .split(|ch: char| ch.is_whitespace() || ch == '+' || ch == '-')
                .filter(|w| !w.is_empty() && *w != "plus")
                .map(|m| match m {
                    "control" | "ctrl" => "Ctrl",
                    "alt" => "Alt",
                    "shift" => "Shift",
                    "windows" | "win" => "Win",
                    "command" | "cmd" => "Cmd",
                    _ => "Option",
                })
                .collect();
            let key_raw = c["key"].to_lowercase();
            let key_raw = key_raw.split_whitespace().collect::<Vec<_>>().join(" ");
            if !explicit {
                // Spoken without "plus": only Ctrl/Alt/Win with a letter,
                // digit, F-key or a few named keys.
                let strong = mods.iter().all(|m| matches!(*m, "Ctrl" | "Alt" | "Win"));
                let single = key_raw.chars().count() == 1;
                let named = matches!(key_raw.as_str(), "tab" | "enter" | "escape" | "esc" | "delete" | "del" | "backspace")
                    || (key_raw.starts_with('f') && key_raw.len() > 1);
                let letter_ok = single && (mods.contains(&"Ctrl") || !matches!(key_raw.as_str(), "a" | "i"));
                if !strong || !(letter_ok || named) {
                    return whole.to_string();
                }
            }
            let key = match key_raw.as_str() {
                "enter" | "return" => "Enter".to_string(),
                "escape" | "esc" => "Esc".to_string(),
                "delete" | "del" => "Delete".to_string(),
                "space" | "spacebar" => "Space".to_string(),
                "page up" => "PgUp".to_string(),
                "page down" => "PgDn".to_string(),
                "print screen" => "PrtSc".to_string(),
                k if k.chars().count() == 1 || k.starts_with('f') => k.to_uppercase(),
                k => capitalize(k),
            };
            let mut parts: Vec<&str> = Vec::new();
            for m in mods {
                if !parts.contains(&m) {
                    parts.push(m);
                }
            }
            // Keep the space that preceded the key if the regex ate it.
            let trailing = if whole.ends_with(char::is_whitespace) { " " } else { "" };
            format!("{}+{key}{trailing}", parts.join("+"))
        })
        .into_owned()
}

fn breaks_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(
            r"(?:^|(?P<end>[.!?])|[,;:])\s*(?:new\s+(?P<kind>paragraph|line)\s*(?:[.,;:!?]+|$)\s*|(?:next\s+|new\s+)?bullet(?:\s+point\b)?(?:\s*[,.:;]+\s*|\s+)(?P<first>[A-Za-z][\w'-]*))",
        )
            .case_insensitive(true)
            .build()
            .expect("valid regex")
    })
}

/// "Milk, new line, eggs" -> "Milk\nEggs". Only when the phrase stands on
/// its own between punctuation, so "add a new line here" is left alone.
fn spoken_breaks(text: &str) -> String {
    let replaced = breaks_regex().replace_all(text, |c: &Captures| {
        let end = c.name("end").map(|m| m.as_str()).unwrap_or("");
        match c.name("kind") {
            Some(k) if k.as_str().eq_ignore_ascii_case("paragraph") => format!("{end}\n\n"),
            Some(_) => format!("{end}\n"),
            // "Bullet points are great" is a sentence, not a command.
            None if c["first"].eq_ignore_ascii_case("points") => c[0].to_string(),
            // The bullet pattern consumed the item's first word; put it back.
            None => format!("{end}\n- {}", &c["first"]),
        }
    });
    if !replaced.contains('\n') {
        return replaced.into_owned();
    }
    let lines: Vec<String> = replaced
        .trim()
        .split('\n')
        .map(|l| {
            let l = l.trim();
            match l.strip_prefix("- ") {
                // List items: capitalised, no trailing full stop.
                Some(item) => format!("- {}", capitalize(item.trim().trim_end_matches(['.', ',']))),
                None => capitalize(l),
            }
        })
        .collect();
    // A dictated list of short items ("new line" only, never paragraphs)
    // loses the full stop on each item.
    let short = !lines.iter().any(|l| l.is_empty() || l.starts_with("- "))
        && lines.iter().all(|l| l.split_whitespace().count() <= 3);
    lines
        .into_iter()
        .map(|l| if short { l.trim_end_matches('.').to_string() } else { l })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Swaps each "heard as" spelling (and any other casing of the word) for the
/// user's spelling, on whole words only.
fn apply_dictionary(text: &str, dictionary: &[Entry]) -> String {
    let mut pairs: Vec<(&str, &str)> = dictionary
        .iter()
        .flat_map(|e| e.aliases.iter().map(|a| (a.as_str(), e.word.as_str())).chain([(e.word.as_str(), e.word.as_str())]))
        .collect();
    pairs.sort_by_key(|(alias, _)| std::cmp::Reverse(alias.len()));
    let mut out = text.to_string();
    for (alias, word) in pairs {
        let pattern = format!(r"\b{}\b", regex::escape(alias));
        if let Ok(re) = RegexBuilder::new(&pattern).case_insensitive(true).build() {
            out = re.replace_all(&out, regex::NoExpand(word)).into_owned();
        }
    }
    out
}

/// A run of one to three words that reads like a Dictionary word without
/// being spelled like it.
struct NearMiss {
    start: usize,
    end: usize,
    word: String,
    same_letters: bool,
}

/// Spans whose letters are within a third of a Dictionary word or one of
/// its "heard as" spellings, starting with the same letter ("Superbass" or
/// "super base" for Supabase). Words under five letters are skipped: too
/// many real words sit that close to them.
fn near_misses(tokens: &[&str], dictionary: &[Entry]) -> Vec<NearMiss> {
    let targets: Vec<(String, &str)> = dictionary
        .iter()
        .flat_map(|e| std::iter::once(&e.word).chain(&e.aliases).map(move |t| (letters(t), e.word.as_str())))
        .filter(|(l, _)| l.chars().count() >= 5)
        .collect();
    if targets.is_empty() {
        return vec![];
    }
    let mut found: Vec<(usize, NearMiss)> = Vec::new();
    for i in 0..tokens.len() {
        for end in i + 1..=(i + 3).min(tokens.len()) {
            // A span never runs across punctuation between its words.
            if tokens[i..end - 1].iter().any(|t| t.ends_with(|c: char| ",.;:!?".contains(c))) {
                break;
            }
            let span = letters(&tokens[i..end].concat());
            let sl = span.chars().count();
            if sl < 4 {
                continue;
            }
            let as_typed = tokens[i..end].iter().map(|t| core(t)).collect::<Vec<_>>().join(" ");
            for (tl, word) in &targets {
                let n = tl.chars().count();
                if span.chars().next() != tl.chars().next() || sl * 3 < n * 2 || sl * 2 > n * 3 {
                    continue;
                }
                // Already right, or the word with an ending ("Supabase's").
                if as_typed.eq_ignore_ascii_case(word) || (sl > n && sl <= n + 2 && span.starts_with(tl.as_str())) {
                    continue;
                }
                // The word itself next to another word ("Supabase this"):
                // swapping that span would delete the other word.
                if end - i > 1 && tokens[i..end].iter().any(|t| letters(t) == *tl || letters(t) == letters(word)) {
                    continue;
                }
                let d = levenshtein(&span, tl);
                let longest = sl.max(n);
                if d * 3 > longest {
                    continue;
                }
                found.push((d * 1000 / longest, NearMiss { start: i, end, word: word.to_string(), same_letters: d == 0 }));
            }
        }
    }
    // Closest first, longer spans breaking ties; overlaps lose.
    found.sort_by_key(|(score, m)| (*score, std::cmp::Reverse(m.end - m.start)));
    let mut taken = vec![false; tokens.len()];
    let mut out = Vec::new();
    for (_, m) in found {
        if taken[m.start..m.end].iter().any(|t| *t) {
            continue;
        }
        taken[m.start..m.end].iter_mut().for_each(|t| *t = true);
        out.push(m);
    }
    out.sort_by_key(|m| m.start);
    out
}

/// Text that still has a near-miss after Light: real words close to a
/// Dictionary word, which only the sentence can settle. Smart takes those.
pub fn has_near_miss(text: &str, dictionary: &[Entry]) -> bool {
    text.lines().any(|l| !near_misses(&l.split_whitespace().collect::<Vec<_>>(), dictionary).is_empty())
}

/// Swaps a near-miss for the Dictionary word when it can't be ordinary
/// words: the letters match exactly ("Supa base" for Supabase), or a word
/// in it isn't English ("Super Biss"). "super base" is two real words, so
/// it's left alone here.
fn fix_near_misses(line: &str, ctx: &Context) -> String {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let misses = near_misses(&tokens, &ctx.dictionary);
    if misses.is_empty() {
        return line.to_string();
    }
    let to_check: Vec<&str> =
        misses.iter().filter(|m| !m.same_letters).flat_map(|m| tokens[m.start..m.end].iter().map(|t| core(t))).collect();
    let known = if to_check.is_empty() { None } else { ctx.known.and_then(|f| f(&to_check)) };
    // Without a spell checker, every word counts as real.
    let is_known = |w: &str| known.as_ref().and_then(|k| to_check.iter().position(|c| *c == w).map(|i| k[i])).unwrap_or(true);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    for m in &misses {
        let sure = m.same_letters || tokens[m.start..m.end].iter().any(|t| !is_known(core(t)));
        if !sure {
            continue;
        }
        out.extend(tokens[i..m.start].iter().map(|t| t.to_string()));
        let (first, last) = (tokens[m.start], tokens[m.end - 1]);
        let lead = &first[..first.len() - first.trim_start_matches(|c: char| !c.is_alphanumeric()).len()];
        out.push(format!("{lead}{}{}", m.word, trailing_punct(last)));
        i = m.end;
    }
    out.extend(tokens[i..].iter().map(|t| t.to_string()));
    out.join(" ")
}

/// Terminals get bare commands; chat apps drop the full stop on one-liners.
fn fit_app(text: &str, app: Option<&str>) -> String {
    let Some(app) = app else { return text.to_string() };
    if text.contains('\n') {
        return text.to_string();
    }
    if TERMINALS.contains(&app) && text.split_whitespace().count() <= 8 {
        return lowercase_first(text.strip_suffix('.').unwrap_or(text));
    }
    if CHATS.contains(&app) && !text.ends_with("..") {
        if let Some(body) = text.strip_suffix('.') {
            if !body.contains(['.', '!', '?']) {
                return body.to_string();
            }
        }
    }
    text.to_string()
}

/// One bench case. Reads both hand-written eval cases (with `expected`) and
/// lines from the transcript log (just `raw` and `app`).
#[derive(Deserialize)]
struct Case {
    raw: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    expected: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// word -> heard-as spellings
    #[serde(default)]
    dictionary: std::collections::BTreeMap<String, Vec<String>>,
}

/// Whitespace-insensitive comparison that still respects line breaks.
fn normalize(s: &str) -> String {
    s.trim()
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn show(s: &str) -> String {
    s.replace('\n', "\\n")
}

/// `offhand --bench-cleanup cases.jsonl [--smart]`: runs every case through
/// Light cleanup (or Smart, which calls the API for cases with a cue) with
/// the user's saved dictionary plus any in the case, and prints failures,
/// changes and timing.
pub fn bench(path: &str, smart: bool) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return println!("can't read {path}: {e}"),
    };
    if smart && !crate::secret::is_set() {
        return println!("Smart needs an API key: save one in Settings, or set ANTHROPIC_API_KEY");
    }
    let (mut total, mut graded, mut passed, mut changed) = (0, 0, 0, 0);
    let mut times = Vec::new();
    let (mut calls, mut fallbacks) = (0, 0);
    let mut call_ms = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let case: Case = match serde_json::from_str(line) {
            Ok(c) => c,
            Err(e) => {
                println!("line {}: not a case ({e})", i + 1);
                continue;
            }
        };
        total += 1;
        // Only the case's own words, so the results don't depend on whose
        // Dictionary the bench runs with.
        let dictionary: Vec<Entry> =
            case.dictionary.iter().map(|(w, a)| Entry { word: w.clone(), aliases: a.clone(), learned: false }).collect();
        let ctx = Context { app: case.app.clone(), light: true, dictionary, known: Some(crate::spell::known) };
        let t = Instant::now();
        let mut out = clean(&case.raw, &ctx);
        times.push(t.elapsed().as_micros());
        if smart {
            let light_text = light(&case.raw, &ctx);
            if crate::smart::needs(&light_text, &ctx.dictionary) {
                calls += 1;
                let t = Instant::now();
                match crate::smart::fix(&light_text, ctx.app.as_deref(), &ctx.dictionary) {
                    Ok(fixed) => out = finish(&fixed, &ctx),
                    Err(e) => {
                        fallbacks += 1;
                        println!("#{} smart failed, using Light: {e}", i + 1);
                    }
                }
                call_ms.push(t.elapsed().as_millis());
            }
        }

        let label = format!(
            "#{} {}{}",
            i + 1,
            case.note.as_deref().unwrap_or(""),
            case.app.as_deref().map(|a| format!(" [{a}]")).unwrap_or_default()
        );
        let did_change = normalize(&out) != normalize(&case.raw);
        if did_change {
            changed += 1;
        }
        match &case.expected {
            Some(expected) => {
                graded += 1;
                if normalize(&out) == normalize(expected) {
                    passed += 1;
                } else {
                    println!("FAIL {label}");
                    println!("  raw:      {}", show(&case.raw));
                    println!("  expected: {}", show(expected));
                    println!("  got:      {}", show(&out));
                }
            }
            None if did_change => {
                println!("CHANGED {label}");
                println!("  raw: {}", show(&case.raw));
                println!("  got: {}", show(&out));
            }
            None => {}
        }
    }
    times.sort_unstable();
    let p50 = times.get(times.len() / 2).copied().unwrap_or(0);
    let max = times.last().copied().unwrap_or(0);
    println!();
    println!("{total} cases: {passed}/{graded} graded cases pass, {changed} changed by cleanup");
    println!("Light time per case: p50 {p50} us, max {max} us");
    if smart {
        call_ms.sort_unstable();
        let at = |q: usize| call_ms.get((call_ms.len() * q / 100).min(call_ms.len().saturating_sub(1))).copied().unwrap_or(0);
        println!(
            "Smart: {calls} of {total} cases called {}, {fallbacks} fell back to Light; p50 {} ms, p90 {} ms, max {} ms",
            crate::smart::MODEL,
            at(50),
            at(90),
            call_ms.last().copied().unwrap_or(0)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light(raw: &str) -> String {
        clean(raw, &Context { app: None, light: true, dictionary: vec![], known: None })
    }

    #[test]
    fn off_is_pass_through() {
        let ctx = Context { app: None, light: false, dictionary: vec![], known: None };
        assert_eq!(clean("  Um, hello.  ", &ctx), "Um, hello.");
    }

    #[test]
    fn fillers_and_their_commas_go() {
        assert_eq!(light("Um, so I think we should, uh, ship it on Friday."), "So I think we should ship it on Friday.");
        assert_eq!(light("The demo went well, um, and the client wants a call."), "The demo went well, and the client wants a call.");
        assert_eq!(light("Ship it, uh."), "Ship it.");
        assert_eq!(light("Uh uh uh, okay, make the repo public."), "Okay, make the repo public.");
        assert_eq!(light("Um."), "");
    }

    #[test]
    fn stretched_and_other_sounds_go() {
        assert_eq!(light("Ahh, that makes sense."), "That makes sense.");
        assert_eq!(light("Ooh, nice one. Uhhh, ship it."), "Nice one. Ship it.");
        assert_eq!(light("So, ummm, hmmm, let's go."), "So let's go.");
        assert_eq!(light("Eh, fine."), "Fine.");
    }

    #[test]
    fn meaningful_sounds_and_acronyms_stay() {
        assert_eq!(light("Oh, I see."), "Oh, I see.");
        assert_eq!(light("Mhm, sounds right."), "Mhm, sounds right.");
        assert_eq!(light("We went to the ER last night."), "We went to the ER last night.");
        assert_eq!(light("Ahead of time, ahem."), "Ahead of time, ahem.");
    }

    #[test]
    fn fillers_inside_words_stay() {
        assert_eq!(light("The umbrella and the hummus."), "The umbrella and the hummus.");
        assert_eq!(light("Uh-huh, that works."), "Uh-huh, that works.");
    }

    #[test]
    fn stutters_collapse_but_real_doubles_stay() {
        assert_eq!(light("I I think the the build is broken."), "I think the build is broken.");
        assert_eq!(light("I know that that works."), "I know that that works.");
        assert_eq!(light("Version 10 10 is out."), "Version 10 10 is out.");
    }

    #[test]
    fn spoken_breaks_only_when_standalone() {
        assert_eq!(light("Thanks. New paragraph. I'll send it."), "Thanks.\n\nI'll send it.");
        assert_eq!(light("Milk, new line, eggs, new line, bread."), "Milk\nEggs\nBread");
        assert_eq!(light("Add a new line to the file."), "Add a new line to the file.");
    }

    #[test]
    fn dictionary_swaps_whole_words() {
        let ctx = Context {
            app: None,
            light: true,
            dictionary: vec![Entry { word: "Supabase".into(), aliases: vec!["supa base".into()], learned: false }],
            known: None,
        };
        assert_eq!(clean("Pushed to supa base and supabase.", &ctx), "Pushed to Supabase and Supabase.");
        assert_eq!(clean("Supabases are nice.", &ctx), "Supabases are nice.");
    }

    /// Stands in for the Windows spell checker.
    fn fake_known(words: &[&str]) -> Option<Vec<bool>> {
        const REAL: &[&str] = &["super", "base", "busy", "army", "built", "near", "coast"];
        Some(words.iter().map(|w| REAL.contains(&w.to_lowercase().as_str())).collect())
    }

    #[test]
    fn close_mishearings_of_dictionary_words() {
        let ctx = Context {
            app: None,
            light: true,
            dictionary: vec![Entry { word: "Supabase".into(), aliases: vec!["Super Biss".into()], learned: true }],
            known: Some(fake_known),
        };
        // Not real words, so they can only be the Dictionary word.
        assert_eq!(clean("I pushed it to superbass this morning.", &ctx), "I pushed it to Supabase this morning.");
        assert_eq!(clean("Push it to super biz.", &ctx), "Push it to Supabase.");
        assert_eq!(clean("Push it to Supa base.", &ctx), "Push it to Supabase.");
        // Real words stay; Smart decides those from the sentence.
        assert_eq!(clean("The army built a super base near the coast.", &ctx), "The army built a super base near the coast.");
        assert!(has_near_miss("The army built a super base near the coast.", &ctx.dictionary));
        assert_eq!(clean("I'm super busy today.", &ctx), "I'm super busy today.");
        assert_eq!(clean("Supabase's docs are good.", &ctx), "Supabase's docs are good.");
        // The word itself never swallows the word next to it.
        assert_eq!(clean("I pushed the fix to Supabase this morning.", &ctx), "I pushed the fix to Supabase this morning.");
        assert_eq!(clean("I pushed the fix to supa base this morning.", &ctx), "I pushed the fix to Supabase this morning.");
        assert!(!has_near_miss("We use Supabase this year.", &ctx.dictionary));
        assert!(!has_near_miss("Sounds good to me.", &ctx.dictionary));
    }

    #[test]
    fn app_context() {
        let term = Context { app: Some("windowsterminal.exe".into()), light: true, dictionary: vec![], known: None };
        assert_eq!(clean("Git status.", &term), "git status");
        let chat = Context { app: Some("slack.exe".into()), light: true, dictionary: vec![], known: None };
        assert_eq!(clean("Sounds good, see you then.", &chat), "Sounds good, see you then");
        assert_eq!(clean("Done. See you then.", &chat), "Done. See you then.");
        assert_eq!(clean("Really?", &chat), "Really?");
    }

    #[test]
    fn repeated_phrases_collapse() {
        assert_eq!(light("You know what, you know what, you know what, let's ship it."), "You know what, let's ship it.");
        assert_eq!(light("We need to, we need to fix the login first."), "We need to fix the login first.");
        assert_eq!(light("It is what it is."), "It is what it is.");
        assert_eq!(light("Bye bye."), "Bye bye.");
    }

    #[test]
    fn key_combos_become_shortcuts() {
        assert_eq!(light("Press control plus C to copy it."), "Press Ctrl+C to copy it.");
        assert_eq!(light("Then hit control shift plus T."), "Then hit Ctrl+Shift+T.");
        assert_eq!(light("Just alt tab back to it."), "Just Alt+Tab back to it.");
        assert_eq!(light("Use control Z to undo."), "Use Ctrl+Z to undo.");
        assert_eq!(light("Close it with alt plus F4."), "Close it with Alt+F4.");
        assert_eq!(light("Open the control panel."), "Open the control panel.");
        assert_eq!(light("Can we shift a meeting to Friday?"), "Can we shift a meeting to Friday?");
        assert_eq!(light("Add alt text to the image."), "Add alt text to the image.");
    }

    #[test]
    fn spoken_quotes_need_both_ends() {
        assert_eq!(light("She said quote ship it today end quote and left."), "She said \"ship it today\" and left.");
        assert_eq!(light("The so-called quote unquote experts."), "The so-called quote unquote experts.");
        assert_eq!(light("Can you quote me a price?"), "Can you quote me a price?");
    }

    #[test]
    fn spoken_bullets() {
        assert_eq!(light("Shopping list. Bullet point eggs. Bullet point milk. Next bullet bread."), "Shopping list.\n- Eggs\n- Milk\n- Bread");
        assert_eq!(light("Add a bullet point here."), "Add a bullet point here.");
        assert_eq!(light("Bullet points are great."), "Bullet points are great.");
    }

    #[test]
    fn normalize_ignores_spacing_but_keeps_lines() {
        assert_eq!(normalize("  a   b \n  c  "), "a b\nc");
        assert_ne!(normalize("a b"), normalize("a\nb"));
    }
}
