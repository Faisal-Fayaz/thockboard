import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface PackInfo {
  id: string;
  name: string;
  origin: string;
  voices: string[];
  peaks: number[][];
}

interface Status {
  sampleRate: number;
  volume: number;
  enabled: boolean;
  pack: number;
  voice: number;
  releaseSound: boolean;
  scrollSound: boolean;
  packs: number;
}

interface Rule {
  pattern: string;
  action: "default" | "quiet" | "mute";
}

const $ = <T extends HTMLElement = HTMLElement>(
  id: string,
): T | null => document.getElementById(id) as T | null;

let packs: PackInfo[] = [];
let pack = 0;
let voice = 0;
let filter = "";
let rules: Rule[] = [];
let hoverTimer: number | undefined;

// ---------------------------------------------------------------------------
// Waveforms
// ---------------------------------------------------------------------------

/** Draws a peak envelope. Painted once per list render, not per frame. */
function drawWave(canvas: HTMLCanvasElement, peaks: number[], sel: boolean): void {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 76;
  const h = canvas.clientHeight || 22;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  const g = canvas.getContext("2d");
  if (!g) return;
  g.scale(dpr, dpr);
  g.clearRect(0, 0, w, h);
  if (!peaks.length) return;

  const mid = h / 2;
  const bw = w / peaks.length;
  g.fillStyle = sel ? "#4f8cff" : "#5b6675";
  for (let i = 0; i < peaks.length; i++) {
    // Half-height envelope, mirrored, so it reads as a waveform not a bar chart.
    const bh = Math.max(1, peaks[i] * (mid - 1));
    g.fillRect(i * bw, mid - bh, Math.max(1, bw - 0.6), bh * 2);
  }
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

function matches(p: PackInfo, v: number): boolean {
  if (!filter) return true;
  const f = filter.toLowerCase();
  return (
    p.name.toLowerCase().includes(f) ||
    (p.voices[v] ?? "").toLowerCase().includes(f)
  );
}

function renderPacks(): void {
  const host = $("packs")!;
  host.innerHTML = "";
  packs.forEach((p, i) => {
    const b = document.createElement("button");
    b.className = i === pack ? "chip on" : "chip";
    const n = p.voices.filter((_, v) => matches(p, v)).length;
    b.innerHTML = `${p.name}<small>${p.origin === "built in" ? "" : p.origin + " · "}${n}</small>`;
    b.onclick = () => {
      pack = i;
      voice = 0;
      void invoke("set_pack", { index: i });
      void invoke("set_voice", { index: 0 });
      render();
    };
    host.appendChild(b);
  });
}

function renderVoices(): void {
  const host = $("voices")!;
  host.innerHTML = "";
  const p = packs[pack];
  if (!p) {
    $("libHint")!.textContent = "No packs found. Drop one into the packs folder.";
    return;
  }

  p.voices.forEach((name, v) => {
    if (!matches(p, v)) return;
    const row = document.createElement("div");
    row.className = v === voice ? "vrow on" : "vrow";
    row.tabIndex = 0;

    // Preview and select are separate controls: nesting a button inside a
    // clickable row makes an invalid, unusable accessibility tree.
    const play = document.createElement("button");
    play.className = "play";
    play.textContent = "▶";
    play.title = "preview";
    play.onclick = (e) => {
      e.stopPropagation();
      void invoke("preview", { index: v });
    };

    const cv = document.createElement("canvas");
    cv.className = "wave";

    const label = document.createElement("div");
    label.className = "vname";
    label.textContent = name;

    const row2 = document.createElement("span");
    row2.className = "vtag";
    row2.textContent = `${v + 1}/${p.voices.length}`;

    row.append(play, cv, label, row2);
    const select = () => {
      voice = v;
      void invoke("set_voice", { index: v });
      void invoke("preview", { index: v });
      render();
    };
    row.onclick = select;
    row.onkeydown = (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        select();
      }
    };
    // Hover-to-preview with a short delay, so sweeping the cursor does not
    // machine-gun the list.
    row.onmouseenter = () => {
      hoverTimer = window.setTimeout(
        () => void invoke("preview", { index: v }),
        220,
      );
    };
    row.onmouseleave = () => {
      if (hoverTimer) window.clearTimeout(hoverTimer);
    };

    host.appendChild(row);
    requestAnimationFrame(() => drawWave(cv, p.peaks[v] ?? [], v === voice));
  });

  const total = p.voices.length;
  $("libHint")!.textContent = `${p.name} · ${total} sound${total === 1 ? "" : "s"} · ${p.origin}`;
}

function render(): void {
  renderPacks();
  renderVoices();
}

// ---------------------------------------------------------------------------
// Perceptual designer
// ---------------------------------------------------------------------------

const SLIDERS = [
  ["sPitch", "vPitch"],
  ["sAttack", "vAttack"],
  ["sRes", "vRes"],
  ["sLoud", "vLoud"],
] as const;

function renderBases(): void {
  const sel = $<HTMLSelectElement>("basePack")!;
  sel.innerHTML = "";
  packs.forEach((p, i) => {
    const o = document.createElement("option");
    o.value = String(i);
    o.textContent = p.name;
    sel.appendChild(o);
  });
  sel.value = String(Math.min(pack, packs.length - 1));
  fillBaseVoices();
}

function fillBaseVoices(): void {
  const sel = $<HTMLSelectElement>("baseVoice")!;
  sel.innerHTML = "";
  const p = packs[Number($<HTMLSelectElement>("basePack")!.value)] ?? packs[0];
  p?.voices.forEach((n, i) => {
    const o = document.createElement("option");
    o.value = String(i);
    o.textContent = n;
    sel.appendChild(o);
  });
}

async function runDesigner(): Promise<void> {
  const p = Number($<HTMLSelectElement>("basePack")!.value);
  const v = Number($<HTMLSelectElement>("baseVoice")!.value);
  const get = (id: string) => Number($<HTMLInputElement>(id)!.value) / 100;
  const res = await invoke<{ ok: boolean; peaks?: number[] }>(
    "designer_preview",
    {
      pack: p,
      voice: v,
      pitch: get("sPitch"),
      attack: get("sAttack"),
      resonance: get("sRes"),
      loudness: get("sLoud"),
    },
  );
  if (res.peaks) {
    const cv = $<HTMLCanvasElement>("designWave")!;
    drawWave(cv, res.peaks, true);
  }
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

function renderRules(): void {
  const host = $("rules")!;
  host.innerHTML = "";
  rules.forEach((r, i) => {
    const row = document.createElement("div");
    row.className = "rule";

    const pat = document.createElement("input");
    pat.className = "txt";
    pat.placeholder = "app name contains...";
    pat.value = r.pattern;
    pat.oninput = () => {
      rules[i].pattern = pat.value;
      void invoke("save_rules", { rules });
    };

    const act = document.createElement("select");
    for (const [v, label] of [
      ["default", "normal"],
      ["quiet", "quieter"],
      ["mute", "silent"],
    ] as const) {
      const o = document.createElement("option");
      o.value = v;
      o.textContent = label;
      act.appendChild(o);
    }
    act.value = r.action;
    act.onchange = () => {
      rules[i].action = act.value as Rule["action"];
      void invoke("save_rules", { rules });
    };

    const del = document.createElement("button");
    del.className = "act";
    del.textContent = "x";
    del.onclick = () => {
      rules.splice(i, 1);
      void invoke("save_rules", { rules });
      renderRules();
    };

    row.append(pat, act, del);
    host.appendChild(row);
  });
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  const st = await invoke<Status>("status");
  pack = st.pack;
  voice = st.voice;

  async function loadPacks(): Promise<void> {
    packs = await invoke<PackInfo[]>("list_packs", { rate: st.sampleRate });
    if (pack >= packs.length) pack = Math.max(0, packs.length - 1);
    render();
    renderBases();
  }
  await loadPacks();

  // Tabs
  document.querySelectorAll<HTMLButtonElement>("nav button").forEach((b) => {
    b.onclick = () => {
      document
        .querySelectorAll("nav button")
        .forEach((x) => x.classList.remove("on"));
      b.classList.add("on");
      for (const t of ["lib", "make", "rules"]) {
        $(`tab-${t}`)!.classList.toggle("hidden", t !== b.dataset.tab);
      }
    };
  });

  $("search")!.addEventListener("input", (e) => {
    filter = (e.target as HTMLInputElement).value;
    render();
  });

  // Footer
  const vol = $<HTMLInputElement>("volume")!;
  vol.value = String(st.volume);
  $("volLabel")!.textContent = `${st.volume}%`;
  vol.oninput = () => {
    $("volLabel")!.textContent = `${vol.value}%`;
    void invoke("set_volume", { pct: Number(vol.value) });
  };

  const toggle = $<HTMLButtonElement>("toggle")!;
  toggle.textContent = st.enabled ? "Mute" : "Unmute";
  toggle.className = st.enabled ? "act" : "act on";
  toggle.onclick = async () => {
    const now = !st.enabled;
    st.enabled = now;
    toggle.textContent = now ? "Mute" : "Unmute";
    toggle.className = now ? "act" : "act on";
    await invoke("set_enabled", { on: now });
  };

  const bindCheck = (
    id: string,
    value: boolean,
    cmd: string,
  ): void => {
    const el = $<HTMLInputElement>(id)!;
    el.checked = value;
    el.onchange = () => void invoke(cmd, { on: el.checked });
  };
  bindCheck("release", st.releaseSound, "set_release_sound");
  bindCheck("scroll", st.scrollSound, "set_scroll_sound");

  $("openPacks")!.onclick = () => void invoke("open_packs_folder");
  $("reload")!.onclick = async () => {
    await invoke("reload_packs");
    await loadPacks();
  };
  const test = $<HTMLButtonElement>("selftest")!;
  test.onclick = async () => {
    test.disabled = true;
    test.textContent = "firing...";
    await invoke("simulate_burst", { count: 8 });
    setTimeout(() => {
      test.disabled = false;
      test.textContent = "self-test";
    }, 1400);
  };

  // Designer
  for (const [sid, vid] of SLIDERS) {
    const s = $<HTMLInputElement>(sid)!;
    s.oninput = () => {
      $(vid)!.textContent = s.value;
    };
    // Preview on release, not on every input event: dragging a slider would
    // otherwise fire hundreds of renders.
    s.onchange = () => void runDesigner();
  }
  $<HTMLSelectElement>("basePack")!.onchange = () => {
    fillBaseVoices();
    void runDesigner();
  };
  $<HTMLSelectElement>("baseVoice")!.onchange = () => void runDesigner();

  $("saveSound")!.onclick = async () => {
    const name = $<HTMLInputElement>("newName")!.value.trim();
    if (!name) {
      $<HTMLInputElement>("newName")!.focus();
      return;
    }
    const get = (id: string) => Number($<HTMLInputElement>(id)!.value) / 100;
    const res = await invoke<{ ok: boolean; error?: string }>(
      "designer_save",
      {
        name,
        pack: Number($<HTMLSelectElement>("basePack")!.value),
        voice: Number($<HTMLSelectElement>("baseVoice")!.value),
        pitch: get("sPitch"),
        attack: get("sAttack"),
        resonance: get("sRes"),
        loudness: get("sLoud"),
      },
    );
    if (res.ok) {
      $<HTMLInputElement>("newName")!.value = "";
      await loadPacks();
    } else {
      alert(`Could not save: ${res.error ?? "unknown error"}`);
    }
  };

  // Rules
  rules = ((await invoke<{ rules: Rule[] }>("list_rules"))?.rules ?? []).slice();
  renderRules();
  $("addRule")!.onclick = () => {
    rules.push({ pattern: "", action: "quiet" });
    void invoke("save_rules", { rules });
    renderRules();
  };

  await listen<string | null>("app-focus", (e) => {
    $("focusApp")!.textContent = e.payload ?? "-";
  });

  $("startBtn")!.onclick = () => {
    $("onboard")!.remove();
    void runDesigner();
  };

  window.addEventListener("resize", () => render());
}

main().catch((err) => {
  const s = $("status");
  if (s) s.textContent = String(err);
  document.body.insertAdjacentHTML(
    "afterbegin",
    `<pre style="color:#e88;padding:16px">${String(err)}</pre>`,
  );
});
