# Kimi Code — tool/command output channel issue (debug notes)

Observed 2026-08-12 in the `/home/pixie/llm-ctl` session. These notes are for
debugging the Kimi CLI/runtime itself, not the project. Project-state details
live in the session history; this file only describes the *channel* failure.

## Symptom
The results of tool calls become unusable. Instead of the real output, the
model receives noise such as `X X X ... X end`, garbled CJK
(`对也对也正 ... 对也对也正`), empty, or a truncated fragment. When the
result is a mangled token the call may or may not have actually executed.

Affects the main interaction loop (agent). Background-task result files read
under the session (`.../sessions/.../agents/main/tasks/*/output.log`) were
*generally* reliable even while foreground results were mangled.

## Triggering / reliability observations (empirical, not causal)
Started intermittently partway through an active session and became worse over
time. It oscillates: a few tool calls render cleanly, then several come back
mangled. Notably:

- Almost always OK: tiny single-purpose commands (`echo`, `test -s FILE && echo`,
  single-line `grep -c`, `ps ... | awk`, `ss -ltn | grep`), `timeout ... /dev/tcp`
  reachability checks.
- Often OK: small foreground commands with 1-3 short lines of output.
- Frequently mangled / dropped:
  - compound multi-command `bash` blocks,
  - `ssh` with nested/escaped double quotes (`sed -i \" ... \"`),
  - heredoc fed over `ssh` (`ssh ... <<'EOF'`),
  - `Write` result confirmation (file itself appeared to be written),
  - larger or multi-line outputs (some `Read`/`Grep`/`cat`),
  - `TaskList`.

## Critical confounder
A mangled result does NOT reliably imply "did not run". At least two cases were
confirmed where the tool call did not execute (target file absent), and at
least two where it did execute despite the mangled display. This makes it
unsafe to infer success/failure from the result token, which is the core of
the problem for infrastructure work.

## Hypotheses worth investigating (unverified)
1. Result bytes are being truncated/corrupted between the tool runner and the
   model context window (e.g. a streaming/UTF-8/size cap issue) more than
   execution failing.
2. Certain content (CJK/ANSI, long JSON, control chars, large single-line dumps)
   trips the renderer, while near-empty output survives — consistent with
   "small single commands fine, big/compound go bad."
3. Accumulated context/output size degrading an underlying buffer over a long
   session (correlates with it appearing partway through and worsening).
4. `ssh`/heredoc with `$`, quotes, and nested escaping exercising a faulty
   shell-arg/render path.

## Suggestions to fix
- Try rendering tool results through a length/codepoint-aware sink and compare
  byte counts vs. what the model sees.
- Look for a sanitizer that mangles CJK or control/EOL sequences.
- Consider resetting/trimming an output ring buffer as a session grows.
- Reproduce with a minimal repro: a command that emits a long one-line JSON,
  then a short `echo`, then a heredoc — see which degrade.

## Repro fixture (safe)
```bash
echo a; sleep 0.2; echo b          # short compound - observe
seq 1 2000 | tr '\n' ','            # long single line
cat <<'EOF' >/dev/null; echo done   # heredoc
x
EOF
ssh localhost 'echo hi' 2>/dev/null||true   # ssh if a loopback ssh exists
```
Compare whether results for the 2nd/3rd/4th come back correct in a row or
degrade/mangle.

---

## Findings (2026-08-12, added by later debug session)

Forensics on `session_079659ce-f4fa-4099-afa4-3c3117764dc1/agents/main/wire.jsonl` (1580 records, cross-scanned for both signatures) narrow the fault to one layer.

### Evidence table: which wire-record kinds carry the garbage

| Record kind (counts in log) | `对也对也正` | `X X X` |
| --- | --- | --- |
| `tool.result` payload (199) | 0 | 0 |
| `llm.request` (186) | 0 | 0 |
| `context.append_message` (assistant / tool / other) | 0 | 0 |
| `context.append_loop_event` — `think` part | 18 | 9 |
| `context.append_loop_event` — `text` part | 1 | 1 |

Conclusion: tool-runner -> disk payloads are byte-clean, and the garbage never
reaches `llm.request` or the persisted conversation. It exists only in the live
streaming loop-event log, inside **model-emitted `think`/`text`** tokens.

### Where the filler sits (smoking gun)

In turn 14 step 37 (and turn 15 step 1, etc.) the `think` text echoes tool-call
XML markup with the filler where a real value belongs, and the corrupting filler
also swallows the leading `<` (the log shows `SML|parameter>` = mangled
`<parameter>`):

```
SML|parameter>
</invoke>
</tool_calls>对也对也正</parameter>
</invoke>
```

The identical 5-char string `对也对也正` (`对也·对也·正`) recurs across many
turns; `X X X ... X end` is the ASCII side of the same filler (scattered isolated
`X` runs in `think`).

### Mechanism (hypothesis, refined)

The same fixed string appearing at the same structural seam every time is a
**deterministic placeholder**, not random byte corruption. It points at the
**streaming / context-append buffer path** (`context.append_loop_event`), i.e.:
- NOT the tool runner (payloads clean),
- NOT the prompt/context sent to the model (llm.request clean),
- a chunk-splitting / escaping bug that swaps a real token for a fixed filler
  when a streamed segment lands on a markup/quoting seam. This is consistent
  with the original "frequently mangled" set (`ssh` nested quotes, heredocs,
  `Write` confirmation) and with the corrupt seams always being at `</tool_calls>`
  / `</parameter>` boundaries.

### Live reproduction (same session, later)
The symptom reproduced live: some `Bash` results rendered as `X X X ... X end`
noise, and at least two short extraction commands **silently never ran** (no
background task registered, no output file created) — directly confirming the
"mangled result != did not run" confounder. Degradation was intermittent and
worsened over the session, matching the original observations.

### Recommended next steps
1. Instrument `context.append_loop_event` streaming: log byte-length and
   code-point identity per chunk vs. what gets appended; flag any mismatch.
2. Minimal repro: emit content heavy in `</tag>` / nested double quotes over
   several streamed chunks; watch the seam between an ending `</tool_calls>`
   and the next `</parameter>`.
3. Grep the runtime binary for the fixed filler (`对也对也正` / repeated `X`) or
   for a default emitted on empty/oversized chunk decode.
4. Make the loop-event log write the canonical part bytes (same source already
   proven clean in `tool.result`) instead of a separately-serialized streaming
   copy, so a bad stream chunk cannot present different bytes than persisted.

Not root-caused in the runtime source (the Kimi CLI binary/source is not in this
workspace); this narrows the failing layer only.

