/**
 * llama-stats: Pi extension -> sets a "llama" status that pi-bar renders.
 *
 * Polls the llm-ctl daemon (/api/status-rollup) and publishes
 *   cache <hit%> · ctx <used%>
 * cache% comes from the most recent recorded turn (last_turn) when present,
 * otherwise from the most recent session's aggregate cache hit rate (Postgres)
 * - so a resumed session shows its conversation's cache state right away.
 * ctx%   = worker slot n_prompt_tokens / n_ctx.
 *
 * Plain text; pi-bar applies the isotope color states in its config.toml.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const ROLLUP_URL = "http://localhost:8082/api/status-rollup";

function pct(n: number, d: number): string {
  if (!d || d <= 0) return "--";
  return ((n / d) * 100).toFixed(1) + "%";
}

function cachePct(lastTurn: any, sessions: any[]): string {
  if (lastTurn) {
    const c = lastTurn.cache_n ?? 0;
    const p = lastTurn.prompt_n ?? 0;
    if (c + p > 0) return pct(c, c + p);
  }
  const s = (sessions || [])[0];
  if (s && typeof s.cache_hit_pct === "number") return s.cache_hit_pct.toFixed(1) + "%";
  return "--";
}

function ctxPct(slots: any[]): string {
  const s = (slots || [])[0];
  if (!s) return "--";
  return pct(s.n_prompt_tokens ?? 0, s.n_ctx ?? 0);
}

function buildText(data: any): string {
  const worker = data?.worker || {};
  return `cache ${cachePct(worker.last_turn, data?.sessions)} · ctx ${ctxPct(worker.slots)}`;
}

export default function (pi: ExtensionAPI) {
  let text = "cache -- · ctx --";
  let timer: ReturnType<typeof setInterval> | null = null;

  async function poll(): Promise<string> {
    try {
      const resp = await fetch(ROLLUP_URL, { signal: AbortSignal.timeout(2000) });
      if (!resp.ok) throw new Error(String(resp.status));
      const data = await resp.json();
      return buildText(data);
    } catch {
      return "cache -- · ctx --";
    }
  }

  // Never let a status-bar poke take pi down (setStatus during a TUI render
  // transition can be racy - e.g. right after resuming a session).
  function safeSetStatus(ctx: any, value: string) {
    try {
      ctx.ui.setStatus("llama", value);
    } catch (e) {
      console.warn("llama-stats setStatus failed:", e);
    }
  }

  // Tell the daemon which pi session is active so proxy-recorded turns are
  // keyed to pi's real session (not the 30-min heuristic that merges them).
  function reportSession(ctx: any) {
    try {
      const sid = ctx?.sessionManager?.getSessionId?.();
      if (typeof sid === "string" && sid) {
        fetch(ROLLUP_URL.replace("/status-rollup", "/session-active"), {
          method: "POST",
          headers: { "Content-Type": "application/json", "X-LLM-CTL": "1" },
          body: JSON.stringify({ id: sid }),
        }).catch(() => {});
      }
    } catch (e) {
      console.warn("llama-stats reportSession error:", e);
    }
  }

  pi.on("session_start", async (event, ctx) => {
    reportSession(ctx);
    safeSetStatus(ctx, "cache -- · ctx --");
    // Let the TUI settle after loading/resuming a session before poking it.
    await new Promise((r) => setTimeout(r, 1000));
    text = await poll();
    safeSetStatus(ctx, text);
    if (timer) clearInterval(timer);
    timer = setInterval(async () => {
      text = await poll();
      safeSetStatus(ctx, text);
    }, 2000);
  });

  pi.on("session_shutdown", async () => {
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
    try {
      fetch(ROLLUP_URL.replace("/status-rollup", "/session-active"), {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-LLM-CTL": "1" },
        body: JSON.stringify({ id: "" }),
      }).catch(() => {});
    } catch {
      /* ignore */
    }
  });
}
