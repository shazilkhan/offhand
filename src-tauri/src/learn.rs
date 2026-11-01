//! Learns spellings from the user's own fixes. A message is often several
//! dictations in a row, proofread and fixed before it's sent, so the text
//! box is watched from the first paste until the message is sent (the
//! pastes leave the box), or two minutes after the last paste. Then every
//! paste is lined up with the box, and a name the user retyped in place
//! ("Neve" -> "Niamh") becomes a Dictionary entry. A chat box empties the
//! moment a message is sent, so the box is read at each Enter (before the
//! app gets it) and twice a second, keeping the latest reading that held
//! still.
//!
//! Privacy: only the box that was pasted into is read, only until then,
//! never a password field, and nothing is kept except the learned word
//! pair. UI Automation calls run on their own thread, so a slow or hung
//! app can't stall dictation.

use crate::cleanup::KnownWords;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationValuePattern,
    UIA_TextPatternId, UIA_ValuePatternId,
};

/// How long after the last paste to keep watching a box that wasn't sent.
const WATCH_FOR: Duration = Duration::from_secs(120);
/// The longest Enter is held back while the field is read.
const ENTER_WAIT: Duration = Duration::from_millis(150);
/// How often the field is read while a paste is watched, to catch the fix
/// before a Send button clears it.
const POLL: Duration = Duration::from_millis(500);
/// Readings in a row that fail before a box is given up on.
const MAX_MISSES: u32 = 6;
const MAX_FIELD_CHARS: i32 = 20_000;
const MAX_LEARN_PER_PASTE: usize = 2;
const MAX_LEARN: usize = 4;

type OnLearn = Box<dyn Fn(String, String) + Send + Sync>;
static ON_LEARN: OnceLock<OnLearn> = OnceLock::new();

enum Cmd {
    /// Another dictation was pasted.
    Paste(String),
    /// Read the field before Enter reaches the app, then reply.
    Snapshot(Sender<()>),
    /// Learning was turned off: stop reading, learn nothing.
    Stop,
}

/// Talks to the watcher while a box is watched.
static ACTIVE: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
/// A box is being watched; lets the keyboard hook skip everything else.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// `on_learn(heard, word)` is called for each spelling the user fixed.
pub fn start(on_learn: OnLearn) {
    let _ = ON_LEARN.set(on_learn);
}

/// Stops watching now (learning was turned off).
pub fn stop() {
    if let Some(tx) = ACTIVE.lock().unwrap().as_ref() {
        let _ = tx.send(Cmd::Stop);
    }
}

/// Called from the keyboard hook when Enter goes down, before the app gets
/// it. A chat box empties as soon as Enter sends the message, so the field
/// is read now, while the user's fix is still in it.
pub fn before_enter() {
    if !WATCHING.load(Ordering::Relaxed) {
        return;
    }
    let Some(tx) = ACTIVE.try_lock().ok().and_then(|g| g.clone()) else { return };
    let (done, wait) = channel();
    if tx.send(Cmd::Snapshot(done)).is_ok() {
        let _ = wait.recv_timeout(ENTER_WAIT);
    }
}

/// Starts watching the focused box for this paste, or adds the paste to
/// the box already watched. The accessibility client lives only on the
/// watcher thread: creating it starts a pool of Windows worker threads, so
/// it's never kept around while idle.
pub fn watch(pasted: &str) {
    let pasted = pasted.trim().to_string();
    let mut active = ACTIVE.lock().unwrap();
    if let Some(tx) = active.as_ref() {
        if tx.send(Cmd::Paste(pasted.clone())).is_ok() {
            return;
        }
    }
    let (tx, rx) = channel::<Cmd>();
    *active = Some(tx);
    WATCHING.store(true, Ordering::Relaxed);
    drop(active);
    let spawned = std::thread::Builder::new().name("learn".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        {
            let automation: Option<IUIAutomation> = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
            let mut next = Some(pasted);
            while let Some(first) = next.take() {
                next = automation.as_ref().and_then(|a| watch_box(a, first, &rx));
                if next.is_none() {
                    // A paste may have come in while this box was wrapped up.
                    // Holding the lock, so none can arrive unseen after this.
                    let mut active = ACTIVE.lock().unwrap();
                    next = rx.try_iter().find_map(|c| if let Cmd::Paste(p) = c { Some(p) } else { None });
                    if next.is_none() {
                        *active = None;
                        WATCHING.store(false, Ordering::Relaxed);
                    }
                }
            }
        }
        CoUninitialize();
    });
    if let Err(e) = spawned {
        log::warn!("correction learning: {e}");
        *ACTIVE.lock().unwrap() = None;
        WATCHING.store(false, Ordering::Relaxed);
    }
}

/// Watches the focused box until the message is sent (the pastes leave
/// the box), the box can't be read, or `WATCH_FOR` passes with no new
/// paste. Then looks for fixes. A paste into a different box ends the
/// watch too, and is returned to be watched next.
unsafe fn watch_box(automation: &IUIAutomation, first: String, rx: &Receiver<Cmd>) -> Option<String> {
    let Ok(el) = automation.GetFocusedElement() else {
        log::info!("learning: couldn't find the text box");
        return None;
    };
    let kind = el.CurrentControlType().map(|t| t.0).unwrap_or(0);
    let class: String = el.CurrentClassName().map(|c| c.to_string()).unwrap_or_default().chars().take(30).collect();
    log::info!("learning: watching a text box (type {kind}, class '{class}')");
    let mut pastes = vec![first];
    let mut deadline = Instant::now() + WATCH_FOR;
    // The latest good reading: the box at an Enter, or once it held still
    // between two reads.
    let mut good: Option<(String, &str)> = None;
    let mut prev: Option<String> = None;
    let mut held = false;
    let mut misses = 0;
    let mut next = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left.min(POLL)) {
            Ok(Cmd::Snapshot(done)) => {
                let text = read_field(&el);
                let _ = done.send(());
                if let Some(text) = text.filter(|t| has_words(t)) {
                    good = Some((text, "at Enter"));
                }
            }
            Ok(Cmd::Paste(p)) => {
                let same = automation
                    .GetFocusedElement()
                    .and_then(|now| automation.CompareElements(&el, &now))
                    .is_ok_and(|b| b.as_bool());
                if !same {
                    next = Some(p);
                    break;
                }
                pastes.push(p);
                deadline = Instant::now() + WATCH_FOR;
            }
            Ok(Cmd::Stop) => return None,
            Err(RecvTimeoutError::Timeout) => {
                let text = read_field(&el);
                misses = if text.is_some() { 0 } else { misses + 1 };
                if misses >= MAX_MISSES && good.is_none() {
                    break;
                }
                if let Some(t) = text.as_ref().filter(|t| prev.as_ref() == Some(t)) {
                    if has_words(t) {
                        good = Some((t.clone(), "while it held still"));
                    }
                    // The dictation was in the box and now isn't: sent or cleared.
                    let holds = pastes.iter().any(|p| holds_paste(p, t));
                    if held && !holds {
                        break;
                    }
                    held |= holds;
                }
                prev = text;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(on_learn) = ON_LEARN.get() {
        check(&pastes, read_field(&el), good, Some(crate::spell::known), on_learn.as_ref());
    }
    next
}

unsafe fn read_field(el: &IUIAutomationElement) -> Option<String> {
    if el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(true) {
        return None;
    }
    if let Ok(vp) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
        if let Ok(v) = vp.CurrentValue() {
            let s = v.to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    let tp = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId).ok()?;
    let range = tp.DocumentRange().ok()?;
    range.GetText(MAX_FIELD_CHARS).ok().map(|s| s.to_string())
}

/// At least half the pasted words are still in the field.
fn holds_paste(pasted: &str, field: &str) -> bool {
    let words: Vec<String> = field.split_whitespace().map(|w| core(w).to_lowercase()).collect();
    let p: Vec<String> = pasted.split_whitespace().map(|w| core(w).to_lowercase()).collect();
    let kept = p.iter().filter(|w| words.contains(w)).count();
    !p.is_empty() && kept * 2 >= p.len()
}

fn has_words(text: &str) -> bool {
    text.split_whitespace().next().is_some()
}

/// Uses the field as it is now, unless the pastes have gone from it (the
/// message was sent, or the box was cleared) and there's an earlier good
/// reading. Logs why nothing was learned, without any of the text.
fn check(
    pastes: &[String],
    now: Option<String>,
    good: Option<(String, &str)>,
    known: Option<KnownWords>,
    on_learn: &dyn Fn(String, String),
) {
    let holds = |f: &str| pastes.iter().any(|p| holds_paste(p, f));
    let (field, when) = match (now, good) {
        (Some(now), _) if holds(&now) => (now, "now"),
        (_, Some(good)) => good,
        (Some(now), None) => (now, "now"),
        (None, None) => return log::info!("learning: couldn't read the text box"),
    };
    let mut found: Vec<(String, String)> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    for p in pastes {
        let (fixes, why) = corrections(p, &field, known);
        for fix in fixes {
            if !found.contains(&fix) {
                found.push(fix);
            }
        }
        skipped.extend(why);
    }
    found.truncate(MAX_LEARN);
    if found.is_empty() {
        let words: usize = pastes.iter().map(|p| p.split_whitespace().count()).sum();
        log::info!(
            "learning: no fix found ({} dictations, {words} words pasted, {} in the box, read {when}){}",
            pastes.len(),
            field.split_whitespace().count(),
            tally(&skipped)
        );
    }
    for (heard, word) in found {
        log::info!("learned a spelling ({} chars, read {when})", word.chars().count());
        on_learn(heard, word);
    }
}

/// "; skipped: 2 x everyday word, 1 x doesn't sound alike", or nothing.
fn tally(reasons: &[&str]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for r in reasons {
        match counts.iter_mut().find(|(c, _)| c == r) {
            Some((_, n)) => *n += 1,
            None => counts.push((r, 1)),
        }
    }
    if counts.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = counts.iter().map(|(r, n)| format!("{n} x {r}")).collect();
    format!("; skipped: {}", parts.join(", "))
}

/// Common words never treated as a misheard spelling; changing one of
/// these is a grammar edit, and learning it would rewrite every future use.
const COMMON: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "so", "to", "of", "in", "on", "at", "for", "with", "is", "it",
    "be", "are", "was", "were", "i", "you", "he", "she", "we", "they", "this", "that", "there", "their",
    "then", "than", "your", "you're", "its", "it's", "not", "no", "yes", "do", "does", "did", "have",
    "has", "had", "can", "will", "would", "should", "could", "my", "me", "our", "us", "if", "as", "by",
    // One-word chat replies people restyle ("Thanks" -> "Thx", "Thank you").
    "thanks", "thank", "thx", "okay", "ok", "sure", "yeah", "yep", "nope", "cool", "great", "nice", "hi",
    "hey", "hello", "bye", "please", "sorry", "lol", "fine", "done", "alright",
];

fn core(w: &str) -> &str {
    w.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Looks like a name or term: starts with a capital (Niamh, Supabase) or
/// has one inside (GitHub, macOS).
fn looks_like_term(w: &str) -> bool {
    let mut chars = w.chars();
    let first_upper = chars.next().is_some_and(char::is_uppercase);
    first_upper || chars.any(char::is_uppercase)
}

/// Edit distance, for "does the fix sound like what was heard?".
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb { prev } else { 1 + prev.min(row[j]).min(row[j + 1]) };
            prev = cur;
        }
    }
    row[b.len()]
}

/// A mishearing keeps the first sound and most of the shape ("Neve" ->
/// "Niamh", "supa base" -> "Supabase"); swapping Friday for Monday is an
/// edit to the meaning, not a spelling fix.
fn sounds_alike(heard: &str, word: &str) -> bool {
    let (a, b) = (letters(heard), letters(word));
    if first_sound(&a).is_none() || first_sound(&a) != first_sound(&b) {
        return false;
    }
    let longest = a.chars().count().max(b.chars().count()).max(1);
    levenshtein(&a, &b) * 10 <= longest * 8
}

/// The first sound, roughly, of lowercase letters: "Kubernetes" heard as
/// "cooper netties", "Xero" as "zero" or "Phoebe" as "feebee" still start
/// alike.
fn first_sound(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let a = chars.next()?;
    Some(match (a, chars.next()) {
        ('p', Some('h')) => 'f',
        ('k' | 'g', Some('n')) => 'n',
        ('w', Some('r')) => 'r',
        ('p', Some('s')) => 's',
        ('c', Some('e' | 'i' | 'y')) => 's',
        ('c' | 'k' | 'q', _) => 'k',
        ('s' | 'z' | 'x', _) => 's',
        ('g' | 'j', _) => 'j',
        ('v', _) => 'f',
        ('a' | 'e' | 'i' | 'o' | 'u' | 'y', _) => 'a',
        (c, _) => c,
    })
}

/// One slip of the finger: a letter added, dropped, changed or swapped.
/// Learning that would teach Offhand the user's own typo.
fn is_typo(heard: &str, word: &str) -> bool {
    let (a, b) = (letters(heard), letters(word));
    let d = levenshtein(&a, &b);
    let sorted = |s: &str| {
        let mut c: Vec<char> = s.chars().collect();
        c.sort_unstable();
        c
    };
    d <= 1 || (d == 2 && sorted(&a) == sorted(&b))
}

/// Lowercase letters and digits only: "Supa base" -> "supabase".
pub(crate) fn letters(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// At either end of the paste the field may go on with other text, so the
/// typed words are the one to three that read most like what was heard.
fn closest_span(heard: &str, f: &[&str], spans: impl Iterator<Item = Range<usize>>) -> Option<Range<usize>> {
    let h = letters(heard);
    spans.min_by_key(|r| levenshtein(&h, &letters(&f[r.clone()].concat())))
}

const SMALL_NUMBERS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve",
    "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS: &[&str] = &["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

/// "fifty" -> 50, "twenty-one" -> 21, "five hundred" -> 500.
fn words_to_number(words: &[&str]) -> Option<u64> {
    let (mut total, mut current, mut any) = (0u64, 0u64, false);
    for w in words.iter().flat_map(|w| w.split('-')) {
        let w = w.to_lowercase();
        if let Some(n) = SMALL_NUMBERS.iter().position(|s| *s == w) {
            current += n as u64;
        } else if let Some(n) = TENS.iter().position(|s| *s == w) {
            current += (n as u64 + 2) * 10;
        } else {
            match w.as_str() {
                "hundred" => current = current.max(1) * 100,
                "thousand" | "million" => {
                    let scale = if w == "thousand" { 1_000 } else { 1_000_000 };
                    total += current.max(1) * scale;
                    current = 0;
                }
                "and" if any => continue,
                _ => return None,
            }
        }
        any = true;
    }
    any.then_some(total + current)
}

/// "50" or "2,000" as a number.
fn digits_value(w: &str) -> Option<u64> {
    let d: String = w.chars().filter(|c| *c != ',').collect();
    if d.is_empty() || !d.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    d.parse().ok()
}

/// A run of pasted words and what the user typed in its place. `left` and
/// `right` index the matched pairs on either side, when there is one.
struct Gap {
    heard: Range<usize>,
    fixed: Range<usize>,
    left: Option<usize>,
    right: Option<usize>,
}

/// A spoken number the user rewrote as digits inside a name ("Nifty fifty
/// Solutions" -> "Nifty 50 Solutions", "Windows eleven" -> "Windows 11").
/// The whole name is learned, so "fifty" elsewhere is left alone. A number
/// with no name around it is just style, and Smart handles that.
fn number_fix(p: &[&str], f: &[&str], pairs: &[(usize, usize)], gap: &Gap) -> Option<(String, String)> {
    if gap.heard.len() > 3 || gap.fixed.len() != 1 {
        return None;
    }
    let value = words_to_number(&p[gap.heard.clone()])?;
    if digits_value(f[gap.fixed.start])? != value {
        return None;
    }
    // Take in up to three name-like words on each side.
    let (mut ps, mut fs) = (gap.heard.start, gap.fixed.start);
    if let Some(mut k) = gap.left {
        for _ in 0..3 {
            let (pk, fk) = pairs[k];
            if pk + 1 != ps || fk + 1 != fs || !looks_like_term(f[fk]) {
                break;
            }
            (ps, fs) = (pk, fk);
            if k == 0 {
                break;
            }
            k -= 1;
        }
    }
    let (mut pe, mut fe) = (gap.heard.end, gap.fixed.end);
    if let Some(mut k) = gap.right {
        for _ in 0..3 {
            if k >= pairs.len() || pairs[k] != (pe, fe) || !looks_like_term(f[fe]) {
                break;
            }
            (pe, fe) = (pe + 1, fe + 1);
            k += 1;
        }
    }
    if ps == gap.heard.start && pe == gap.heard.end {
        return None;
    }
    Some((p[ps..pe].join(" "), f[fs..fe].join(" ")))
}

/// A misheard name or term swapped for the right one, one to three words
/// on either side: "Neve" -> "Niamh", "supa base" -> "Supabase",
/// "Superbase" -> "Supa base", "tory" -> "tauri". The fix must be a name
/// (a capital) or a term (not an English word, and not just a typo), keep
/// the first sound, and not replace a common word: retyping a one-word
/// "Thanks" as "Thx" is a style change, not a mishearing.
fn term_fix(p: &[&str], f: &[&str], gap: &Gap, unknown: &dyn Fn(&[&str]) -> bool) -> Result<(String, String), &'static str> {
    let heard = &p[gap.heard.clone()];
    let fixed = &f[gap.fixed.clone()];
    if heard.len() > 3 || fixed.len() > 3 {
        return Err("more than three words");
    }
    let heard_text = heard.join(" ");
    let word = fixed.join(" ");
    if letters(&word).chars().count() < 3 {
        return Err("too short");
    }
    if !fixed.iter().all(|w| w.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '\'')) {
        return Err("has symbols");
    }
    if heard.len() == 1 && COMMON.contains(&heard[0].to_lowercase().as_str()) {
        return Err("a common word changed");
    }
    if !sounds_alike(&heard_text, &word) {
        return Err("doesn't sound alike");
    }
    let name = fixed.iter().any(|w| looks_like_term(w));
    if !name && (is_typo(&heard_text, &word) || !unknown(fixed)) {
        return Err("everyday word");
    }
    Ok((heard_text, word))
}

/// The same word with its capitals fixed into a name: mixed case ("github"
/// -> "GitHub", "macos" -> "macOS"), or a capital mid-sentence on a word
/// that isn't English ("shazil" -> "Shazil"). All-caps edits are skipped:
/// "VERY" is usually emphasis, not a spelling.
fn case_fix(heard: &str, word: &str, mid_sentence: bool, unknown: &dyn Fn(&[&str]) -> bool) -> Option<(String, String)> {
    if heard == word || word.chars().count() < 3 || COMMON.contains(&heard.to_lowercase().as_str()) {
        return None;
    }
    let inner_cap = word.chars().skip(1).any(char::is_uppercase) && word.chars().any(char::is_lowercase);
    let capitalised = word.chars().next().is_some_and(char::is_uppercase)
        && word.chars().skip(1).all(|c| !c.is_uppercase())
        && heard.chars().all(|c| !c.is_uppercase());
    let ok = inner_cap || (capitalised && mid_sentence && unknown(&[heard]));
    ok.then(|| (heard.to_string(), word.to_string()))
}

/// What the user fixed in place, inside a sentence that otherwise survived
/// intact: a misheard name or term, a spoken number inside a name, or a
/// name's capitals. `known` tells English words from names and terms.
/// Also returns why each other edit was passed over, for the log.
fn corrections(pasted: &str, field: &str, known: Option<KnownWords>) -> (Vec<(String, String)>, Vec<&'static str>) {
    // Without a spell checker every word counts as English, so only
    // capitalised names are learned.
    let unknown = |words: &[&str]| known.and_then(|k| k(words)).is_some_and(|v| v.contains(&false));
    let p: Vec<&str> = pasted.split_whitespace().map(core).filter(|w| !w.is_empty()).collect();
    let f_raw: Vec<&str> = field.split_whitespace().filter(|w| !core(w).is_empty()).collect();
    // A paste of one or two words has no words around a fix to line up
    // with, so it's only checked when the box holds little else ("Supa
    // base" sent on its own and fixed to "Supabase").
    let short = p.len() < 3;
    if p.is_empty() || p.len() > 300 || f_raw.is_empty() {
        return (vec![], vec![]);
    }
    if short && f_raw.len() > p.len() + 1 {
        return (vec![], vec!["short dictation in a longer box"]);
    }
    let f_raw = &f_raw[f_raw.len().saturating_sub(4000)..];
    let f_all: Vec<&str> = f_raw.iter().map(|w| core(w)).collect();
    let f = &f_all[..];
    let (n, m) = (p.len(), f.len());
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    // Longest common subsequence, then walk it back into matched pairs.
    let mut dp = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if eq(p[i], f[j]) { dp[at(i + 1, j + 1)] + 1 } else { dp[at(i + 1, j)].max(dp[at(i, j + 1)]) };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if eq(p[i], f[j]) {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // The pasted sentence must have survived mostly intact, or it was a rewrite.
    if !short && pairs.len() * 10 < n * 7 {
        return (vec![], vec!["rewritten or gone"]);
    }

    // The runs between matched words. At either end of the paste the field
    // may go on with other text, so only one typed word is considered there.
    let mut gaps = Vec::new();
    if pairs.is_empty() {
        gaps.push(Gap { heard: 0..n, fixed: 0..m, left: None, right: None });
    } else {
        let (first, last) = (pairs[0], pairs[pairs.len() - 1]);
        if first.0 > 0 && first.1 > 0 {
            let heard = p[..first.0].join(" ");
            if let Some(fixed) = closest_span(&heard, f, (1..=first.1.min(3)).map(|k| first.1 - k..first.1)) {
                gaps.push(Gap { heard: 0..first.0, fixed, left: None, right: Some(0) });
            }
        }
        for (k, w) in pairs.windows(2).enumerate() {
            let ((pi, fj), (pi2, fj2)) = (w[0], w[1]);
            gaps.push(Gap { heard: pi + 1..pi2, fixed: fj + 1..fj2, left: Some(k), right: Some(k + 1) });
        }
        if last.0 + 1 < n && last.1 + 1 < m {
            let heard = p[last.0 + 1..].join(" ");
            let spans = (1..=(m - last.1 - 1).min(3)).map(|k| last.1 + 1..last.1 + 1 + k);
            if let Some(fixed) = closest_span(&heard, f, spans) {
                gaps.push(Gap { heard: last.0 + 1..n, fixed, left: Some(pairs.len() - 1), right: None });
            }
        }
    }

    let mut found = Vec::new();
    let mut skipped = Vec::new();
    for g in gaps.iter().filter(|g| !g.heard.is_empty() && !g.fixed.is_empty()) {
        if let Some(fix) = number_fix(&p, f, &pairs, g) {
            found.push(fix);
            continue;
        }
        match term_fix(&p, f, g, &unknown) {
            Ok(fix) => found.push(fix),
            Err(why) => skipped.push(why),
        }
    }
    let mid_sentence = |j: usize| j > 0 && !f_raw[j - 1].ends_with(['.', '!', '?', ':']);
    found.extend(pairs.iter().filter_map(|&(pi, fj)| case_fix(p[pi], f[fj], mid_sentence(fj), &unknown)));
    found.dedup();
    found.truncate(MAX_LEARN_PER_PASTE);
    (found, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_corrections(pasted: &str, field: &str, known: Option<KnownWords>) -> Vec<(String, String)> {
        corrections(pasted, field, known).0
    }

    #[test]
    fn learns_a_name_fixed_in_place() {
        let got = find_corrections("Thanks Neve, talk soon about the launch.", "Thanks Niamh, talk soon about the launch.", None);
        assert_eq!(got, vec![("Neve".to_string(), "Niamh".to_string())]);
    }

    #[test]
    fn learns_a_two_word_mishearing() {
        let got = find_corrections("I pushed the fix to supa base this morning.", "I pushed the fix to Supabase this morning.", None);
        assert_eq!(got, vec![("supa base".to_string(), "Supabase".to_string())]);
    }

    #[test]
    fn finds_the_paste_inside_a_longer_field() {
        let field = "Earlier notes here. Then I deployed it to Tauri builds today. More text after.";
        let got = find_corrections("Then I deployed it to Tory builds today.", field, None);
        assert_eq!(got, vec![("Tory".to_string(), "Tauri".to_string())]);
    }

    #[test]
    fn learns_a_number_inside_a_name() {
        let got = find_corrections("I work at Nifty fifty Solutions now.", "I work at Nifty 50 Solutions now.", None);
        assert_eq!(got, vec![("Nifty fifty Solutions".to_string(), "Nifty 50 Solutions".to_string())]);
        let got = find_corrections("We finally moved to Windows eleven.", "We finally moved to Windows 11. Next line.", None);
        assert_eq!(got, vec![("Windows eleven".to_string(), "Windows 11".to_string())]);
    }

    #[test]
    fn learns_a_short_message_fixed_on_its_own() {
        let got = find_corrections("Supa base", "Supabase", None);
        assert_eq!(got, vec![("Supa base".to_string(), "Supabase".to_string())]);
        let got = find_corrections("Nifty fifty.", "Nifty 50", None);
        assert_eq!(got, vec![("Nifty fifty".to_string(), "Nifty 50".to_string())]);
        // A one-word restyle, or a short paste inside other typed text, isn't learned.
        assert!(find_corrections("Thanks.", "Thx", None).is_empty());
        assert!(find_corrections("Thanks.", "Thank you", None).is_empty());
        // Speech recognition can be far off on one word; the fix still counts.
        let got = find_corrections("Superbus.", "Supabase", None);
        assert_eq!(got, vec![("Superbus".to_string(), "Supabase".to_string())]);
        assert!(find_corrections("Supa base", "Earlier I wrote about Supabase", None).is_empty());
    }

    #[test]
    fn learns_one_word_split_into_two() {
        let got = find_corrections("Superbase.", "Supa base", None);
        assert_eq!(got, vec![("Superbase".to_string(), "Supa base".to_string())]);
        let got = find_corrections("We moved the auth to superbase.", "We moved the auth to Supa base. More text", None);
        assert_eq!(got, vec![("superbase".to_string(), "Supa base".to_string())]);
        let got = find_corrections("Superbase", "Supabase", None);
        assert_eq!(got, vec![("Superbase".to_string(), "Supabase".to_string())]);
        // Split into ordinary lowercase words: a grammar edit, not a name.
        assert!(find_corrections("We should meet anyway tomorrow.", "We should meet any way tomorrow.", None).is_empty());
    }

    #[test]
    fn a_sent_message_is_judged_by_the_reading_at_enter() {
        assert!(holds_paste("I pushed it to supa base.", "I pushed it to Supabase."));
        assert!(!holds_paste("I pushed it to supa base.", ""));
        assert!(!holds_paste("Supa base", "Supabase"));
    }

    #[test]
    fn a_plain_number_edit_is_not_learned() {
        assert!(find_corrections("We have fifty signups today.", "We have 50 signups today.", None).is_empty());
    }

    #[test]
    fn learns_a_fix_at_either_end() {
        let got = find_corrections("Neve, can you call me back?", "Niamh, can you call me back?", None);
        assert_eq!(got, vec![("Neve".to_string(), "Niamh".to_string())]);
    }

    #[test]
    fn learns_mixed_case_names_but_not_emphasis() {
        let got = find_corrections("I pushed it to github this morning.", "I pushed it to GitHub this morning.", None);
        assert_eq!(got, vec![("github".to_string(), "GitHub".to_string())]);
        assert!(find_corrections("This is very important for us.", "This is VERY important for US.", None).is_empty());
    }

    #[test]
    fn number_words() {
        assert_eq!(words_to_number(&["fifty"]), Some(50));
        assert_eq!(words_to_number(&["twenty-one"]), Some(21));
        assert_eq!(words_to_number(&["five", "hundred"]), Some(500));
        assert_eq!(words_to_number(&["two", "thousand", "and", "five"]), Some(2005));
        assert_eq!(words_to_number(&["lots"]), None);
        assert_eq!(digits_value("2,000"), Some(2000));
    }

    #[test]
    fn ignores_grammar_edits_and_rewrites() {
        assert!(find_corrections("I think there going to ship it.", "I think they're going to ship it.", None).is_empty());
        assert!(find_corrections("Send it over by Friday please.", "Send it over by Monday please.", None).is_empty());
        assert!(find_corrections("We should ship it on Friday.", "Totally different message now.", None).is_empty());
        assert!(find_corrections("We should ship it on Friday.", "We should ship it on Friday.", None).is_empty());
    }

    /// A tiny English dictionary; any other word is a name or a term.
    fn fake_known(words: &[&str]) -> Option<Vec<bool>> {
        const ENGLISH: &[&str] = &["tory", "really", "apple", "floated", "floating"];
        Some(words.iter().map(|w| ENGLISH.contains(&w.to_lowercase().as_str())).collect())
    }

    #[test]
    fn learns_lowercase_terms_but_not_typos_or_everyday_words() {
        let got = find_corrections("I built the app with tory and rust.", "I built the app with tauri and rust.", Some(fake_known));
        assert_eq!(got, vec![("tory".to_string(), "tauri".to_string())]);
        // The user's own slip, or one English word for another, isn't a spelling.
        assert!(find_corrections("That is really good news.", "That is realy good news.", Some(fake_known)).is_empty());
        assert!(find_corrections("The pill floated up again.", "The pill floating up again.", Some(fake_known)).is_empty());
        // Without a spell checker, only capitalised names are learned.
        assert!(find_corrections("I built the app with tory and rust.", "I built the app with tauri and rust.", None).is_empty());
    }

    #[test]
    fn a_mishearing_can_start_with_another_letter_of_the_same_sound() {
        let got = find_corrections("We moved the cluster to cooper netties last week.", "We moved the cluster to Kubernetes last week.", None);
        assert_eq!(got, vec![("cooper netties".to_string(), "Kubernetes".to_string())]);
        let got = find_corrections("I sent the draft to feebee yesterday.", "I sent the draft to Phoebe yesterday.", None);
        assert_eq!(got, vec![("feebee".to_string(), "Phoebe".to_string())]);
    }

    #[test]
    fn learns_a_name_capitalised_mid_sentence() {
        let got = find_corrections("I talked to shazil about the launch.", "I talked to Shazil about the launch.", Some(fake_known));
        assert_eq!(got, vec![("shazil".to_string(), "Shazil".to_string())]);
        // An English word, or a capital at the start of a sentence, isn't a name.
        assert!(find_corrections("I bought an apple today.", "I bought an Apple today.", Some(fake_known)).is_empty());
        assert!(find_corrections("All set. shazil will call.", "All set. Shazil will call.", Some(fake_known)).is_empty());
    }

    #[test]
    fn checks_every_dictation_in_the_message() {
        // Two dictations, then the first one fixed just before sending.
        let pastes = vec!["We moved the auth to superbase last week.".to_string(), "Next we ship the billing page.".to_string()];
        let field = "We moved the auth to Supabase last week. Next we ship the billing page.";
        let learned = Mutex::new(Vec::new());
        check(&pastes, Some(field.into()), None, None, &|h, w| learned.lock().unwrap().push((h, w)));
        assert_eq!(*learned.lock().unwrap(), vec![("superbase".to_string(), "Supabase".to_string())]);
        // Sent: the box is empty now, so the reading at Enter is used.
        let learned = Mutex::new(Vec::new());
        check(&pastes, Some("\n".into()), Some((field.into(), "at Enter")), None, &|h, w| learned.lock().unwrap().push((h, w)));
        assert_eq!(learned.lock().unwrap().len(), 1);
    }

    #[test]
    fn says_why_edits_were_skipped() {
        let (_, why) = corrections("We will meet them on Friday at noon.", "We will meet them on Monday at noon.", None);
        assert_eq!(tally(&why), "; skipped: 1 x doesn't sound alike");
        assert_eq!(tally(&[]), "");
    }
}
