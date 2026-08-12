// llm-ctl panel: plain fetch polling (Qwen-recommended over SSE for single user).
"use strict";

const HDR = { "X-LLM-CTL": "1" };
const $ = (id) => document.getElementById(id);

let lastLog = "";

async function api(path, opts = {}) {
  const res = await fetch(path, { ...opts, headers: { ...HDR, ...(opts.headers || {}) } });
  if (!res.ok) throw new Error(`${path} -> HTTP ${res.status}`);
  return res.json();
}

async function fetchStatus() {
  try {
    const s = await api("/api/status-rollup");
    render(s);
  } catch (e) {
    console.warn("poll failed", e);
  }
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
const asInt = (n) => Number.isFinite(Number(n)) ? String(Math.round(Number(n))) : "0";
const asFloat = (n) => Number.isFinite(Number(n)) ? String(Number(n).toFixed(1)) : "0.0";

function render(s) {
  // worker
  const w = s.worker || {};
  const st = String(w.status || "Stopped");
  const dot = $("worker-dot");
  dot.className = "inline-block w-2.5 h-2.5 rounded-full " +
    (st.startsWith("Ready") ? "bg-lime" : st.startsWith("Starting") ? "bg-cyan" : st.startsWith("Crashed") ? "bg-red" : "bg-faint");
  $("worker-status").textContent = `${st} · ${w.model || "no model"}${w.pid ? " · pid " + w.pid : ""}`;
  const cross = s.cross_tier || {};
  $("pve-status").textContent = "pve " + (cross.pve ? "up" : "down");
  $("pg-status").textContent = "pg " + (cross.postgres ? "up" : "down");
  $("pve-status").className = "text-" + (cross.pve ? "lime" : "red");
  $("pg-status").className = "text-" + (cross.postgres ? "lime" : "red");

  // models
  const activeId = (s.models || []).find((m) => m.active)?.id;
  $("models").innerHTML = (s.models || []).map((m) => {
    const isActive = m.id === activeId;
    return `<div class="flex items-center justify-between bg-raised border border-line rounded px-2 py-1.5">
      <div class="min-w-0">
        <div class="truncate ${isActive ? "text-lime" : "text-text"}">${esc(m.id)}${isActive ? " ★" : ""}</div>
        <div class="text-faint text-[10px] truncate">${esc(m.description || m.model)}</div>
      </div>
      <button data-act="${isActive ? "stop" : "start"}" data-id="${esc(m.id)}"
        class="shrink-0 ml-2 rounded px-2 py-0.5 text-xs font-bold border
        ${isActive ? "border-red text-red hover:bg-red hover:text-black" : "border-lime text-lime hover:bg-lime hover:text-black"}">
        ${isActive ? "stop" : "start"}</button>
    </div>`;
  }).join("");

  // sessions
  $("sessions").innerHTML = (s.sessions || []).map((r) => {
    const cache = r.cache_hit_pct || 0;
    const cls = cache >= 50 ? "text-lime" : cache >= 20 ? "text-magenta" : "text-red";
    return `<tr class="border-b border-line/50">
      <td class="py-1 pr-2 truncate max-w-32 text-cyan">${esc(r.id)}</td>
      <td class="py-1 pr-2 text-dim truncate max-w-24">${esc(r.model || "")}</td>
      <td class="py-1 pr-2 text-right">${asInt(r.requests)}</td>
      <td class="py-1 pr-2 text-right">${asInt(r.prompt_tokens)}</td>
      <td class="py-1 pr-2 text-right">${asInt(r.completion_tokens)}</td>
      <td class="py-1 text-right ${cls}">${cache.toFixed(1)}%</td>
    </tr>`;
  }).join("") || '<tr><td colspan="6" class="py-2 text-faint">no sessions</td></tr>';

  // jobs
  renderJob("hf", s.download, "HF download");
  renderJob("rebuild", s.rebuild, "Rebuild");
  const sha = s.rebuild && s.rebuild.after_sha ? `${s.rebuild.before_sha || ""} → ${s.rebuild.after_sha}` : (s.rebuild && s.rebuild.before_sha) || "";
  $("rebuild-sha").textContent = sha ? "sha " + sha.slice(0, 40) : "";
  $("rebuild-sha2").textContent = sha ? sha : "";
  $("hf-repo").disabled = !!s.download && s.download.status === "running";
  $("hf-go").disabled = !!s.download && s.download.status === "running";
  $("hf-list").disabled = !!s.download && s.download.status === "running";
  $("hf-files").disabled = !!s.download && s.download.status === "running";
  $("rebuild-go").disabled = !!s.rebuild && s.rebuild.status === "running";

  // llama build info + previous builds
  const li = s.llama || {};
  $("llama-info").textContent = `commit ${esc(li.commit)} · ${esc(li.branch)} · ${esc(li.binary)}`;
  const blds = s.builds || [];
  $("builds").innerHTML = blds.length
    ? blds.map((b) => {
        const t = b.at ? new Date(b.at * 1000).toLocaleString() : "";
        return `<div class="flex justify-between gap-2 py-0.5 ${b.ok ? "text-lime" : "text-red"}">
          <span>${t}</span>
          <span class="truncate">${esc(b.before_sha || "—")} → ${esc(b.after_sha || "—")}</span>
          <span>${b.ok ? "ok" : "failed"}</span></div>`;
      }).join("")
    : "no recent builds";
}

function renderJob(name, job, label) {
  const el = $(name + "-job");
  if (!job || job.status === "idle") { el.textContent = ""; return; }
  const lines = (job.log || []).slice(-8).join("\n");
  el.textContent = `${label}: ${job.status}${job.error ? " (" + job.error + ")" : ""}\n${lines}`;
  // unified log tail
  if (name === "hf" || (name === "rebuild" && (!window._activeJob || window._activeJob === name))) {
    // prefer the most recently updated job for the big log pane
  }
  if (job.status === "running") {
    window._activeJob = name;
    showLog(job);
  } else if (window._activeJob === name) {
    showLog(job);
    window._activeJob = null;
  }
}

function showLog(job) {
  const text = (job.log || []).join("\n");
  if (text !== lastLog) {
    lastLog = text;
    const pre = $("joblog");
    pre.textContent = text || "(no output yet)";
    pre.scrollTop = pre.scrollHeight;
  }
}

async function doAction(path, btn) {
  if (btn) { btn.disabled = true; btn.textContent = "…"; }
  try { await api(path, { method: "POST" }); }
  catch (e) { console.error(e); }
  finally { if (btn) { btn.disabled = false; btn.textContent = "↻"; } fetchStatus(); }
}

document.addEventListener("click", (e) => {
  const b = e.target.closest("[data-act]");
  if (!b) return;
  const id = b.dataset.id;
  b.disabled = true;
  doAction(b.dataset.act === "start" ? `/api/models/${encodeURIComponent(id)}/start` : "/api/models/stop", b);
});

$("hf-list").addEventListener("click", async () => {
  const repo = $("hf-repo").value.trim();
  if (!repo) return;
  const sel = $("hf-files");
  sel.innerHTML = '<option value="">(loading…)</option>';
  try {
    const r = await api(`/api/hf/files?repo=${encodeURIComponent(repo)}`);
    const files = r.files || [];
    sel.innerHTML =
      '<option value="">choose a GGUF…</option>' +
      files.map((f) => `<option value="${esc(f.path)}">${esc(f.path)} (${asFloat(f.size_mb)} MB)</option>`).join("");
    $("hf-job").textContent = files.length ? `${files.length} GGUF files` : "no .gguf files in this repo";
  } catch (e) {
    sel.innerHTML = '<option value="">choose a GGUF…</option>';
    $("hf-job").textContent = "list failed: " + e.message;
  }
});

$("hf-go").addEventListener("click", async () => {
  const repo = $("hf-repo").value.trim();
  if (!repo) return;
  const btn = $("hf-go");
  btn.disabled = true;
  $("hf-job").textContent = "starting…";
  try {
    await fetch("/api/hf/download", {
      method: "POST",
      headers: { "Content-Type": "application/json", ...HDR },
      body: JSON.stringify({
        repo,
        include: $("hf-files").value || $("hf-include").value.trim() || "*q4_k_m.gguf",
      }),
    });
  } catch (e) {
    console.error(e);
  } finally {
    btn.disabled = false;
    fetchStatus();
  }
});

$("rebuild-go").addEventListener("click", () => doAction("/api/rebuild", $("rebuild-go")));

fetchStatus();
setInterval(fetchStatus, 2500);
