# onedrive-sync — architecture & language recommendation

*Researched 2026-06. Audience: the tool's owner. Goal: decide whether/how to make
the tool stable, low-overhead, and professional — and in what language.*

---

## TL;DR

- **The tool is now stable** (this session fixed the crash classes). "Stable" is
  largely *done*. The real driver to change anything is **low overhead** and
  **a UI that can't crash** — and those are *structural*, not bugs you can patch out
  of PowerShell + WPF.
- **There are two separate decisions, with very different confidence:**
  1. **Architecture** *(high confidence)* — move to the proven shape for this class
     of tool: a **single compiled binary** that runs as a small background
     **daemon + CLI**, exposes a **local HTTP API**, and serves a **management UI as a
     local web page** (open in your normal browser). `rclone` and `git` stay as
     subprocesses (they already do the real work). A **thin tray** talks to the daemon.
  2. **Language** *(softer, your call)* — **Go** is my lean (it is literally the
     language of the proven peers: Syncthing, rclone, Tailscale). **C#/.NET** is a
     strong alternative if you value .NET familiarity. **Rust** if you want maximum
     rigor. The *architecture* works in all three.
- **Do not big-bang rewrite.** The 3,500 lines of PowerShell are a **specification**
  of hard-won edge cases. Port incrementally and run **Go and PowerShell side-by-side
  on real folders until the new one provably matches** (strangler, never a cutover).
- **Recommended first concrete step:** a ~1-day **throwaway spike** to de-risk the
  unknowns before porting any real logic. Details at the end.

---

## 1. Why this is even on the table (the diagnosis)

Almost every crash and papercut this session was *structural to the stack*, not a
one-off bug:

| Pain (this session) | Root cause | Patchable? |
|---|---|---|
| `Refresh-Data` / `Update-ButtonStates` "not recognized" crashes | PowerShell **dynamic scoping** — a function's visibility depends on the live call stack, so UI event handlers can't see helpers | No — inherent to PS |
| "works in 5.1, breaks in 7" (and vice-versa) | Two PowerShell runtimes; `ConvertFrom-Json`, `Set-Content` BOM, StrictMode differ | No — inherent |
| `Unexpected token` from a no-BOM file | 5.1 reads UTF-8 as ANSI without a BOM | No — inherent |
| WPF re-entrancy killing the tray | WPF + a background refresh mutating a grid under a modal | Mitigatable, not eliminable |
| ~hundreds of ms startup **every** scheduled run | PowerShell launches + dot-sources ~3,500 lines every 30 min | **No — this is the overhead** |

The first four are why it was *unstable* (now worked around). The last one is why it
can't be *low-overhead* while it stays PowerShell: a script host must start and parse
the whole core on every run. A compiled binary starts in single-digit milliseconds and
a resident daemon doesn't re-parse anything.

**Conclusion:** "stable" was achievable in PowerShell (and we did it). "Low overhead"
and "a UI that structurally cannot crash" are **not** — they require leaving PS + WPF.

---

## 2. What the research says

### a. C# NativeAOT can't carry the existing WPF
The appealing idea — "compile the current WPF tray to a tiny native exe" — does not
work. WPF and WinForms are **disabled for trimming/Native AOT** in the .NET SDK (WPF
relies on reflection; WinForms on COM marshalling). So C# *with the existing GUI* means
a heavier, runtime-dependent deployment, and it keeps a native-GUI crash surface.
([Microsoft Learn: trimming incompatibilities](https://learn.microsoft.com/en-us/dotnet/core/deploying/trimming/incompatibilities),
[Native AOT overview](https://learn.microsoft.com/en-us/dotnet/core/deploying/native-aot/))

### b. The proven peers all share one shape
Syncthing, rclone, and Tailscale — the mature tools in exactly this category
(background file sync / networking daemons) — are all **Go**, and Syncthing's design is
the template:

- a **daemon** runs in the background and does the sync logic;
- it serves a **web management UI on `127.0.0.1`**, opened in a normal browser;
- **tray helpers** are thin and talk to the daemon over a **REST API + an event/long-poll
  API**; the robust core never depends on the UI.

([Syncthing GUI docs](https://docs.syncthing.net/intro/gui.html),
[Syncthing dev intro](https://docs.syncthing.net/dev/intro.html),
[Web GUI architecture](https://deepwiki.com/syncthing/syncthing/4.1-web-gui))

**The key insight:** *a local web UI deletes the entire WPF crash class we fought all
session.* There is no dynamic-scope dispatch, no modal re-entrancy, no XAML — just a
page that calls a local API. The UI can crash and the daemon (the part that matters)
keeps syncing.

### c. rclone is happy to be driven as a subprocess
The mature, low-risk integration is exactly what we do today: **invoke the `rclone`
binary**. (There is also `librclone` — a C-shared lib — and an `rc` HTTP API, but the
subprocess approach keeps rclone's battle-tested code at arm's length and is what
projects like the rclone-bisync-manager do.)
([rclone rc API](https://rclone.org/rc/), [rclone bisync](https://rclone.org/bisync/))

### d. Tooling exists for whichever language you pick
- **Go:** mature tray libs — [fyne-io/systray](https://github.com/fyne-io/systray),
  [gogpu/systray](https://github.com/gogpu/systray) (pure Go, no CGO). If you ever want
  a native window instead of the browser, [Wails v3](https://v3.wails.io/) bundles a
  WebView2 UI + tray into one binary — **but it is alpha; keep it off the critical path**
  (see §4).
- **C#:** native tray is trivial (`NotifyIcon`); serve the web UI with the built-in
  Kestrel/minimal-API; AOT-friendly if the UI is web, not WPF.
- **Rust:** [tray-icon](https://crates.io/crates/tray-icon) + a web UI, or
  [Tauri](https://v2.tauri.app/) for a packaged native-webview app.

---

## 3. Recommended architecture (high confidence, language-agnostic)

```
        ┌─────────────────────────────────────────────┐
        │  ods (one compiled binary)                   │
        │                                              │
        │   ┌──────────┐   subprocess   ┌──────────┐   │
        │   │  engine  │ ─────────────► │  rclone  │   │
        │   │  (sync   │ ─────────────► │   git    │   │
        │   │   logic, │                └──────────┘   │
        │   │   state, │   serves                      │
        │   │  filters)│ ◄──── localhost REST + events │
        │   └────┬─────┘                               │
        │        │ subcommands (ods sync, ods status…) │
        └────────┼─────────────────────────────────────┘
                 │                         ▲
       scheduled │                         │ localhost:PORT
        task /   │                         │
       on-logon  ▼                         │
            (runs `ods sync`)        ┌──────────────┐   ┌──────────────────┐
                                     │  thin tray   │   │ browser web UI   │
                                     │ (icon+menu)  │   │ (management page) │
                                     └──────────────┘   └──────────────────┘
```

- **One binary, several roles:** `ods sync` (the scheduled run — fast, no host
  startup), `ods serve` (the resident daemon + web UI + tray), `ods status|pause|…`
  (CLI). Same code, no PowerShell.
- **Management UI = a web page** served by the daemon, opened in your browser. No
  native-GUI framework on the critical path → **the crash class is gone, by construction.**
- **Tray = thin:** an icon, a menu (Sync now / Pause / Open dashboard / Quit) that calls
  the local API. If it ever dies, syncing is unaffected.
- **rclone + git stay subprocesses** — same proven approach as today.
- **Low overhead:** scheduled runs are a native exe (ms, not ~hundreds of ms); the
  resident daemon holds state instead of re-reading/parsing every run.

This is the Syncthing model, adapted to "rclone bisync + git per project."

---

## 4. Language: the softer decision (your call)

The architecture above is identical regardless. The language choice is about **what you
want to maintain**, not about correctness.

| | **Go** *(my lean)* | **C# / .NET** | **Rust** |
|---|---|---|---|
| Fit for this tool | Exact match — the peers (Syncthing/rclone/Tailscale) are Go | Strong; most Windows-native | Strong; most rigorous |
| Single binary / overhead | Yes, static, tiny startup | Yes (self-contained / AOT if UI is web) | Yes, smallest, no GC |
| Subprocess + HTTP + JSON | Excellent, stdlib | Excellent | Excellent |
| Tray on Windows | Good (fyne/gogpu systray) | Trivial (`NotifyIcon`) | Good (tray-icon) |
| Learning curve / maintainability | Low — simple by design | Low **if you already know .NET** | High — borrow checker |
| Native window option | Wails v3 (alpha) | WinUI/WPF (heavy) or web | Tauri |

- **Pick Go** if you want the boring, proven, single-static-binary path that the whole
  category already walks.
- **Pick C#** if your own familiarity with .NET matters more — it's a fine choice the
  moment the UI is web instead of WPF.
- **Pick Rust** only if you specifically want compiler-enforced rigor and don't mind the
  ramp.

**On "different languages for different functions":** the *right* split is **one compiled
backend + a web frontend** (Go/C#/Rust + HTML/CSS/JS) — which is itself "two languages,"
and exactly what Syncthing does. I'd **advise against** mixing, say, Rust + Go + C# for
different *engine* parts: the only perf-critical work is already inside `rclone` (native),
so a polyglot backend would add complexity and *hurt* the "professional/maintainable"
goal, not help it.

---

## 5. The 3,500 lines are a *spec*, not a liability

The PowerShell isn't the asset — the **edge cases encoded in it** are: tombstones &
catalog conflict-merge, the delete-brake percentage, git-aware filter generation, the
mtime-seed clock-skew warnings, the "filters-changed → resync" recovery, the
protected-root / overlap guards, and every fix from this session. A from-scratch rewrite
**re-derives all of it and re-introduces those bugs.**

So the migration is a **strangler with overlap, never a cutover**:

1. Treat `tests/run-tests.ps1` (22 checks) + this session's documented edge cases as the
   **conformance spec**. Port them to the new language *first*.
2. For a transition period, **run Go and PowerShell side-by-side on real folders** (Go in
   `--dry-run`/shadow mode) and diff their decisions until the Go version provably matches.
3. Only then cut the scheduled task over to the Go binary, one capability at a time.

---

## 6. Phased plan & honest cost

| Phase | What | Rough effort* |
|---|---|---|
| **0. Spike** | Throwaway Go binary: sync **one** project via rclone subprocess + serve a one-page status UI + show a tray icon, on your machine. De-risks: rclone-from-Go, the browser/tray story, your toolchain. | ~1 day |
| **1. Engine + CLI** | Port the core (discovery, filters, state, git-aware logic, the delete-brake, conflict scan) to `ods sync` / `ods status`. Port the test suite. Run shadow-mode beside PowerShell until it matches. | ~1–2 weeks |
| **2. Daemon + web UI** | `ods serve`: local REST API + the management web page (replaces the WPF window). | ~1 week |
| **3. Tray + cutover** | Thin tray over the API; switch the scheduled task to `ods`; retire the PowerShell + WPF. | ~3–5 days |
| **4. Packaging** | Single signed installer (Inno Setup/MSIX), auto-update, uninstaller. | ~2–3 days |

*\*Solo, part-time, will vary. The point isn't the number — it's that this is **weeks, not
hours**, and most of the value/risk is Phase 1 (re-encoding the edge cases correctly).*

**Not doing the rewrite is a legitimate option.** The tool works now. If "low overhead"
and "uncrashable UI" aren't worth weeks to you, hardening the current PowerShell further
is defensible — but it will always pay the script-host startup cost and carry a native-GUI
surface.

---

## 7. Decisions for you

1. **Rewrite, or keep hardening PowerShell?** (Driver = low overhead + uncrashable UI.)
2. **If rewrite: which language** — Go (my lean), C# (.NET familiarity), or Rust?
3. **Start with the 1-day spike?** It validates the riskiest unknowns before any real
   logic is ported, and it's throwaway if you decide against it.

I won't start a rewrite without your call on the above — it's the most hard-to-reverse
step in this project, and the language is genuinely your preference to set.
