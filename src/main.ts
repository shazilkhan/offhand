import "./style.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Settings = {
  hotkey: string;
  mic: string | null;
  muteWhileDictating: boolean;
  sounds: boolean;
  trailingSpace: boolean;
  keepModelMins: number;
  startWithWindows: boolean;
  keepTranscripts: boolean;
  historyDays: number;
  handsFreeMode: boolean;
  learnCorrections: boolean;
  pillMode: "always" | "windowed" | "talking" | "never";
  pillX: number | null;
  pillY: number | null;
  cleanup: "off" | "light" | "smart";
};

type Entry = { word: string; aliases: string[]; learned?: boolean };

type HistoryItem = { id: number; text: string; raw: string; app: string; at: number; audioSecs: number; tookMs: number };

type UiState = {
  settings: Settings;
  status: "idle" | "recording" | "transcribing" | "needsModel";
  modelInstalled: boolean;
  modelLoaded: boolean;
  modelBytes: number;
  download: { active: boolean; done: number; total: number; error: string | null };
  recent: HistoryItem[];
  historyTotal: number;
  dictionary: Entry[];
  week: { words: number; wpm: number; savedMins: number };
  transcriptCount: number;
  hotkeys: [string, string][];
  version: string;
  smartKey: boolean;
};

type Toggle =
  | "muteWhileDictating"
  | "sounds"
  | "startWithWindows"
  | "trailingSpace"
  | "keepTranscripts"
  | "handsFreeMode"
  | "learnCorrections";

const VIEWS = ["home", "history", "dictionary", "settings"] as const;
type View = (typeof VIEWS)[number];
const PAGE = 100;

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const all = <T extends HTMLElement = HTMLElement>(sel: string) => [...document.querySelectorAll<T>(sel)];

let state: UiState | null = null;
let view: View = "home";
let copied = "";
let micsLoaded = false;
const hist = { query: "", items: [] as HistoryItem[], total: 0, loadedTotal: -1 };

const CLEAN_NOTES = {
  off: "Pasted exactly as heard.",
  light:
    "Removes um, uh and ah, drops repeated words and phrases, fixes names from your Dictionary, writes spoken shortcuts as Ctrl+C, and understands \"new line\", \"bullet point\" and \"quote … end quote\". Instant, and it runs on this PC.",
  smart:
    "Everything Light does, plus an AI pass for what depends on meaning: numbers (\"fifty signups\" becomes 50 signups), corrections (\"Tuesday, no wait, Wednesday\"), counted lists, and email and web addresses. Only the text goes to Anthropic, never audio, and only when it has one of those. If a reply takes more than 3 seconds, the Light text is pasted.",
};

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(label: string, cls: string, onClick: () => void, aria?: string) {
  const b = el("button", cls, label);
  b.type = "button";
  if (aria) b.setAttribute("aria-label", aria);
  b.onclick = onClick;
  return b;
}

function hotkeyLabel(s: UiState) {
  return s.hotkeys.find(([id]) => id === s.settings.hotkey)?.[1] ?? s.settings.hotkey;
}

function mb(bytes: number) {
  return `${Math.round(bytes / 1_000_000)} MB`;
}

function dayLabel(at: number) {
  const d = new Date(at * 1000);
  const today = new Date();
  const start = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((start(today) - start(d)) / 86_400_000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  return d.toLocaleDateString([], {
    weekday: "long",
    month: "short",
    day: "numeric",
    year: d.getFullYear() === today.getFullYear() ? undefined : "numeric",
  });
}

function setView(next: string) {
  view = (VIEWS as readonly string[]).includes(next) ? (next as View) : "home";
  for (const v of VIEWS) $(`view-${v}`).hidden = v !== view;
  for (const a of all<HTMLAnchorElement>("nav .nav")) {
    const on = a.dataset.view === view;
    a.classList.toggle("on", on);
    if (on) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  if (location.hash !== `#${view}`) history.replaceState(null, "", `#${view}`);
  $("view-home").parentElement!.scrollTop = 0;
  if (view === "history") loadHistory(true);
}

async function save(patch: Partial<Settings>) {
  if (!state) return;
  try {
    await invoke("save_settings", { settings: { ...state.settings, ...patch } });
  } catch (e) {
    alert(`Couldn't save that: ${e}`);
  }
  await refresh();
}

async function saveDictionary(entries: Entry[]) {
  try {
    await invoke("save_dictionary", { entries: entries.map((e) => ({ learned: false, ...e })) });
  } catch (e) {
    alert(`Couldn't save the dictionary: ${e}`);
  }
  await refresh();
}

async function copy(text: string, key: string) {
  await invoke("copy_text", { text });
  copied = key;
  if (state) renderRecent(state);
  renderHistoryList();
}

/** One dictation row, shared by Home and History. */
function row(h: HistoryItem, deletable: boolean) {
  const li = el("li");
  li.append(el("span", "label from", h.app || "—"), el("p", undefined, h.text));
  const meta = el("span", "meta");
  const when = new Date(h.at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  meta.append(el("span", "time", when));
  const key = `${h.id}:text`;
  meta.append(
    button(copied === key ? "Copied" : "Copy", "tbtn" + (copied === key ? " done" : ""), () => copy(h.text, key), `Copy: ${h.text}`),
  );
  if (h.raw && h.raw !== h.text) {
    const rkey = `${h.id}:raw`;
    const orig = button(
      copied === rkey ? "Copied" : "Original",
      "tbtn" + (copied === rkey ? " done" : ""),
      () => copy(h.raw, rkey),
      "Copy what you said before cleanup",
    );
    meta.append(orig);
  }
  if (deletable) {
    meta.append(
      button(
        "Delete",
        "tbtn danger",
        async () => {
          await invoke("delete_history", { id: h.id });
          hist.items = hist.items.filter((x) => x.id !== h.id);
          hist.total = Math.max(0, hist.total - 1);
          renderHistoryList();
        },
        "Delete this dictation",
      ),
    );
  }
  li.append(meta);
  return li;
}

function renderStatus(s: UiState) {
  const text = { idle: "Ready", recording: "Listening", transcribing: "Writing", needsModel: "Setting up" }[s.status];
  $("status-text").textContent = text;
  $("status-bar").className = "status-bar" + (s.status === "recording" || s.status === "transcribing" ? " live" : "");
  $("status-mic").textContent = s.settings.mic ?? "Windows default mic";
  $("status-model").textContent = !s.modelInstalled
    ? "Downloading the speech model"
    : s.modelLoaded
      ? "Model loaded"
      : "Model loads when you talk";
}

function renderRecent(s: UiState) {
  $("recent").replaceChildren(...s.recent.map((h) => row(h, false)));
  $("recent-empty").hidden = s.recent.length > 0;
  $("see-all").hidden = s.historyTotal === 0;
  $("see-all").textContent = `See all ${s.historyTotal.toLocaleString()}`;
}

function renderHome(s: UiState) {
  $("home-key").textContent = hotkeyLabel(s);
  $("home-verb").textContent = s.settings.handsFreeMode ? "Tap" : "Hold";
  $("home-tail").textContent = s.settings.handsFreeMode ? "to talk, tap again to finish." : "and talk.";
  const dl = s.download;
  $("setup").hidden = s.modelInstalled;
  if (!s.modelInstalled) {
    const pct = dl.total ? Math.floor((dl.done / dl.total) * 100) : 0;
    $("setup-bar").style.width = `${pct}%`;
    $("setup-text").textContent = dl.error
      ? `The download stopped: ${dl.error}`
      : dl.active
        ? `Downloading the speech model: ${mb(dl.done)} of ${mb(dl.total)} (${pct}%). This happens once.`
        : `Offhand needs a one-time ${mb(s.modelBytes)} download.`;
    $("setup-retry").hidden = dl.active;
    $("setup-retry").textContent = dl.error ? "Try again" : "Download";
  }
  $("stat-words").textContent = s.week.words.toLocaleString();
  $("stat-wpm").textContent = String(s.week.wpm);
  $("stat-saved").textContent = String(s.week.savedMins);
  renderRecent(s);
}

async function loadHistory(reset: boolean) {
  const offset = reset ? 0 : hist.items.length;
  const page = await invoke<{ items: HistoryItem[]; total: number }>("get_history", {
    query: hist.query,
    offset,
    limit: PAGE,
  });
  hist.items = reset ? page.items : [...hist.items, ...page.items];
  hist.total = page.total;
  hist.loadedTotal = state?.historyTotal ?? -1;
  renderHistoryList();
}

function renderHistoryList() {
  const groups: HTMLElement[] = [];
  let current = "";
  let list: HTMLUListElement | null = null;
  for (const h of hist.items) {
    const label = dayLabel(h.at);
    if (label !== current || !list) {
      current = label;
      const head = el("h2", "label day", label);
      list = el("ul", "rows");
      groups.push(head, list);
    }
    list.append(row(h, true));
  }
  $("hist-groups").replaceChildren(...groups);
  const kept = state?.settings.keepTranscripts ?? true;
  $("hist-sub").textContent = kept
    ? "Every dictation, newest first. Kept on this PC only."
    : "History is off, so this is only this session. Turn on \"Keep history on this PC\" in Settings to keep every dictation.";
  $("hist-count").textContent = hist.query
    ? `${hist.total.toLocaleString()} ${hist.total === 1 ? "match" : "matches"}`
    : `${hist.total.toLocaleString()} ${hist.total === 1 ? "dictation" : "dictations"}`;
  $("hist-empty").hidden = hist.items.length > 0;
  $("hist-empty").textContent = hist.query ? "Nothing matches that." : "Nothing yet. Hold your shortcut and talk.";
  $("hist-more").hidden = hist.items.length >= hist.total;
}

function renderDictionary(s: UiState) {
  const n = s.dictionary.length;
  $("words-h").textContent = n === 0 ? "No words yet" : n === 1 ? "1 word" : `${n} words`;
  $("words").replaceChildren(
    ...s.dictionary.map((e, i) => {
      const li = el("li", "word");
      const w = el("span", "w", e.word);
      if (e.learned) {
        const badge = el("span", "badge", "learned");
        badge.title = "Added automatically from a correction you made";
        w.append(badge);
      }
      li.append(w, el("span", "heard", e.aliases.length ? e.aliases.join("  ·  ") : "spelling only"));
      li.append(button("Remove", "tbtn danger", () => saveDictionary(s.dictionary.filter((_, j) => j !== i)), `Remove ${e.word}`));
      return li;
    }),
  );
  $("dict-note").textContent =
    s.settings.cleanup !== "off"
      ? "Cleanup swaps the \"heard as\" spellings as you dictate, and fixes the word's capitals. Words marked learned came from your own fixes, like a name you retyped or a number you changed to digits."
      : "Text cleanup is off, so these aren't applied. Turn on Light in Settings.";
}

function renderSettings(s: UiState) {
  const st = s.settings;
  $("key-label").textContent = hotkeyLabel(s);
  const keySeg = $("key-seg");
  if (keySeg.childElementCount === 0) {
    for (const [id, label] of s.hotkeys) {
      const b = button(label, "", () => {
        $("key-pick").hidden = true;
        $("key-change").textContent = "Change";
        $("key-change").classList.remove("on");
        save({ hotkey: id });
      });
      b.dataset.key = id;
      keySeg.append(b);
    }
  }
  for (const b of all<HTMLButtonElement>("#key-seg button")) {
    b.classList.toggle("on", b.dataset.key === st.hotkey);
    b.setAttribute("aria-pressed", String(b.dataset.key === st.hotkey));
  }
  for (const b of all<HTMLButtonElement>("[data-pill]")) {
    b.classList.toggle("on", b.dataset.pill === st.pillMode);
    b.setAttribute("aria-pressed", String(b.dataset.pill === st.pillMode));
  }
  $("pill-pos").textContent = st.pillX === null ? "Bottom centre of your main screen" : "Where you last left it";
  $<HTMLSelectElement>("mic").value = st.mic ?? "";
  for (const t of all<HTMLInputElement>("[data-toggle]")) t.checked = st[t.dataset.toggle as Toggle];
  $<HTMLSelectElement>("keep").value = String(st.keepModelMins);
  const dl = s.download;
  $("model-state").textContent = s.modelInstalled
    ? s.modelLoaded
      ? "Loaded"
      : "Installed"
    : dl.active && dl.total
      ? `${Math.floor((dl.done / dl.total) * 100)}%`
      : "Not downloaded";
  for (const b of all<HTMLButtonElement>("[data-clean]")) {
    b.classList.toggle("on", b.dataset.clean === st.cleanup);
    b.setAttribute("aria-pressed", String(b.dataset.clean === st.cleanup));
  }
  $("clean-note").textContent = CLEAN_NOTES[st.cleanup] ?? "";
  $("smart-key").hidden = st.cleanup !== "smart";
  $("key-entry").hidden = s.smartKey;
  $("key-saved").hidden = !s.smartKey;
  $("days-row").hidden = !st.keepTranscripts;
  $<HTMLSelectElement>("days").value = String(st.historyDays);
  const n = s.transcriptCount;
  $("tx-row").hidden = !st.keepTranscripts && n === 0;
  $("tx-count").textContent = n === 1 ? "1 dictation in history" : `${n.toLocaleString()} dictations in history`;
  $("tx-clear").hidden = n === 0;
  $("version").textContent = `Offhand ${s.version}`;
}

function render(s: UiState) {
  state = s;
  renderStatus(s);
  renderHome(s);
  renderDictionary(s);
  renderSettings(s);
  // A new dictation arrived while History is open: show it.
  if (view === "history" && s.historyTotal !== hist.loadedTotal && !hist.query) loadHistory(true);
}

async function refresh() {
  render(await invoke<UiState>("get_state"));
}

async function loadMics() {
  if (micsLoaded) return;
  micsLoaded = true;
  const mics = await invoke<string[]>("list_mics");
  const select = $<HTMLSelectElement>("mic");
  select.replaceChildren(new Option("Windows default", ""), ...mics.map((m) => new Option(m, m)));
  if (state) select.value = state.settings.mic ?? "";
}

// Mic test: open the mic for a few seconds and draw its level.
const BARS = 24;
$("meter").replaceChildren(...Array.from({ length: BARS }, () => el("i")));
let testTimer = 0;
let testEnd = 0;

function drawMeter(level: number) {
  const lit = Math.round(level * BARS);
  all("#meter i").forEach((b, i) => b.classList.toggle("lit", i < lit));
}

async function stopTest() {
  window.clearInterval(testTimer);
  testTimer = 0;
  drawMeter(0);
  $("mic-test").textContent = "Test";
  $("mic-test").classList.remove("on");
  $("level-note").textContent = "Test it to see the mic hearing you";
  await invoke("mic_test", { on: false });
}

$("mic-test").onclick = async () => {
  if (testTimer) return stopTest();
  await invoke("mic_test", { on: true });
  $("mic-test").textContent = "Stop";
  $("mic-test").classList.add("on");
  $("level-note").textContent = "Say something";
  testEnd = Date.now() + 6000;
  testTimer = window.setInterval(async () => {
    if (Date.now() > testEnd) return stopTest();
    drawMeter(await invoke<number>("mic_level"));
  }, 70);
};

// Wiring
const win = getCurrentWindow();
$("win-min").onclick = () => win.minimize();
$("win-max").onclick = () => win.toggleMaximize();
$("win-close").onclick = () => win.close();

document.addEventListener("click", (e) => {
  const link = (e.target as HTMLElement).closest<HTMLElement>("[data-view]");
  if (link) {
    e.preventDefault();
    setView(link.dataset.view!);
  }
});

let searchTimer = 0;
$("search").addEventListener("input", () => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(() => {
    hist.query = $<HTMLInputElement>("search").value;
    loadHistory(true);
  }, 150);
});
$("hist-more").onclick = () => loadHistory(false);
$("setup-retry").onclick = () => invoke("download_model").then(refresh);

$<HTMLFormElement>("dict-form").addEventListener("submit", (e) => {
  e.preventDefault();
  if (!state) return;
  const word = $<HTMLInputElement>("dict-word").value.trim();
  if (!word) return;
  const aliases = $<HTMLInputElement>("dict-alias")
    .value.split(",")
    .map((a) => a.trim())
    .filter(Boolean);
  $<HTMLInputElement>("dict-word").value = "";
  $<HTMLInputElement>("dict-alias").value = "";
  saveDictionary([{ word, aliases }, ...state.dictionary.filter((d) => d.word.toLowerCase() !== word.toLowerCase())]);
});

$("key-change").onclick = () => {
  const open = $("key-pick").hidden === true;
  $("key-pick").hidden = !open;
  $("key-change").textContent = open ? "Cancel" : "Change";
  $("key-change").classList.toggle("on", open);
};

for (const b of all<HTMLButtonElement>("[data-pill]")) b.onclick = () => save({ pillMode: b.dataset.pill as Settings["pillMode"] });
for (const b of all<HTMLButtonElement>("[data-clean]")) b.onclick = () => save({ cleanup: b.dataset.clean as Settings["cleanup"] });
for (const t of all<HTMLInputElement>("[data-toggle]")) t.onchange = () => save({ [t.dataset.toggle as Toggle]: t.checked });
$("mic").onchange = () => save({ mic: $<HTMLSelectElement>("mic").value || null });
$("keep").onchange = () => save({ keepModelMins: Number($<HTMLSelectElement>("keep").value) });
$("days").onchange = () => save({ historyDays: Number($<HTMLSelectElement>("days").value) });
function keyNote(text: string, bad = false) {
  const note = $("key-note");
  note.hidden = !text;
  note.textContent = text;
  note.classList.toggle("bad", bad);
}

async function testKey() {
  keyNote("Testing…");
  try {
    const r = await invoke<{ ms: number; text: string }>("test_smart");
    keyNote(`Works. Took ${(r.ms / 1000).toFixed(1)} s and wrote: ${r.text}`);
  } catch (e) {
    keyNote(`That didn't work: ${e}`, true);
  }
}

async function saveKey() {
  const input = $<HTMLInputElement>("key-input");
  const key = input.value.trim();
  if (!key) return input.focus();
  try {
    await invoke("save_api_key", { key });
  } catch (e) {
    return keyNote(`Couldn't save the key: ${e}`, true);
  }
  input.value = "";
  await refresh();
  await testKey();
}

$("key-save").onclick = saveKey;
$("key-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") saveKey();
});
$("key-get").onclick = () => invoke("open_key_page");
$("key-test").onclick = testKey;
$("key-remove").onclick = async () => {
  await invoke("remove_api_key");
  keyNote("");
  await refresh();
};
$("pill-reset").onclick = () => invoke("reset_pill").then(refresh);
$("tx-show").onclick = () => invoke("show_transcripts");
$("tx-clear").onclick = async () => {
  const n = state?.transcriptCount ?? 0;
  if (!confirm(`Delete all ${n} ${n === 1 ? "dictation" : "dictations"} from history? This can't be undone.`)) return;
  await invoke("clear_transcripts");
  hist.items = [];
  hist.total = 0;
  renderHistoryList();
  await refresh();
};

listen("state-changed", refresh);
listen<string>("navigate", (e) => setView(e.payload));

refresh().then(() => {
  setView(location.hash.slice(1));
  loadMics();
});
