# AGENTS.md

Rust project (`vanguard_os`, edition 2021): an egui/eframe "fake OS" with a boot animation, a
desktop, and a terminal that handles slash-commands, calls a remote LLM, and writes/executes
generated scripts. It is a port of a Python prototype — `Cargo.toml` comments name the Python
module each dependency replaces. Comments and log strings are Russian/English mixed; keep that
style.

**Two binaries + one lib** (since Stage 2, 2026-10-13). The engine is a separate process from
the GUI:

- `vanguard_os` (default for `cargo run`) — the **shell**: egui render, AI stream to Ollama,
  and an IPC *client*. Entry `src/main.rs`.
- `vanguard_core` — the **core**: TCP server on `127.0.0.1:7331` doing the heavy/system work
  (slash-commands, `/run`, `rustc -O` compilation, telemetry). Entry `src/bin/core.rs`. The
  shell **autostarts it on demand** (see `ipc_client.rs`), so `cargo run` alone is enough.
- `vanguard_os_lib` (`src/lib.rs`) — shared code: the IPC protocol, `ai_core`, and the engine
  moved to `src/core/{shell,executor,telemetry}.rs`. Both binaries are thin wrappers over it,
  which is what keeps the protocol from drifting apart.

The split exists for one measured reason: `rustc -O` compilation used to run on the UI thread
and froze the render for seconds. Everything else stayed in the shell because it is not heavy
(AI streaming is SSE on a background thread already).

## Commands

- `cargo check` — the only fast verification. There are no tests, no lint/fmt/typecheck config,
  no CI, no codegen. `cargo test` finds nothing.
  - **On Windows, `cargo check 2>&1` from PowerShell exits with code 1 even on success**: `2>&1`
    merges cargo's (empty) stderr into the PowerShell error stream, and PS 5.1's
    `NativeCommandError` then sets `$LASTEXITCODE = 1`. The tell is the line
    `Finished dev profile ...` with no `error`/`warning:` lines above it — trust that, not the
    exit code. To get a truthful exit code, redirect to a file: `cargo check > $env:TEMP\c.txt`
    then inspect `$LASTEXITCODE`, or grep the file for `^(error|warning)`.
- `cargo run` — with `default-run = "vanguard_os"` this builds and opens the maximized GUI
  window (blocks; Windows-targeted). It does **not** launch the core: the shell spawns
  `vanguard_core.exe` itself the first time it needs it (`ipc_client::spawn_core_process`),
  looking for it next to its own exe — so both binaries must be built (`cargo build` builds
  both) and run from `target/debug` (or any dir holding both). You can also start the core
  yourself first; the shell will just connect instead of spawning.
- `cargo run --bin vanguard_core` — run the core alone, no GUI. It prints `vanguard_core:
  не удалось занять 127.0.0.1:7331` to stderr and exits if the port is taken. Handy for
  debugging IPC: it keeps running after the shell closes, so a second `cargo run` reuses it.
- UI is verified manually only. The boot animation auto-advances to `Ready` and then **stops
  until you press a key**; the desktop and the command input line only appear after that. A
  window sitting on "PRESS ANY KEY" is normal, not a hang.
  - Do **not** try to drive the GUI with `PostMessage`/`SendInput` from a script: egui/winit
    ignores injected input unless the window has real OS focus, and `SetForegroundWindow` from
    a background process is blocked by Windows. Typing into it that way produces garbage (VK
    codes interpreted through the *Russian* layout) and can leak keystrokes into whatever
    window *does* have focus — including a game. Test IPC against the core directly over TCP
    (`{"cmd":"ping"}` on 127.0.0.1:7331) and leave GUI clicks to a human.

## Build state — re-run before trusting this

`cargo check` (and `cargo build`) pass with **no warnings**, covering the lib plus both bins.

`app.rs`, `ai_core.rs` and `Cargo.toml` change often, so re-run `cargo check` yourself rather
than assuming this snapshot still holds. `ipc.rs` is the one file where a "harmless" edit breaks
the wire silently — see the IPC section below.

## No console output

Progress in the GUI is shown **only** in the chat (`ai_text_area`); the boot banner and the
`println!`-duplicates in `send_ai_query` were removed. The one remaining `println!` is a `None`
fallback in `src/core/shell.rs` for when `shell.log_tx` is unset, and `ai_core.rs:43` matches
only because `println!` appears inside the prompt's JSON example string.

The **core** (`src/bin/core.rs`) does write to stderr with `eprintln!` — but only for "couldn't
bind 7331" / "accept failed". That is deliberate: when autostarted by the shell it is spawned
with `Stdio::null()` and `CREATE_NO_WINDOW`, so nothing reaches a console. Run
`vanguard_core.exe` by hand from a terminal and those lines are the only diagnostics there is.

## AI request pipeline — read before touching `app.rs`

The wiring spans several places and is easy to half-break:

- `VanguardOSApp::new` creates the only channel (`mpsc::channel::<AiEvent>()`), keeping the tx in
  `ai_event_tx` and the rx in `ai_channel_rx`. Keep both in sync with
  `ai_core::spawn_ai_request(.., Sender<AiEvent>, ..)`.
- Flow: input line (in `update()`) → `send_ai_query` → `shell.handle_command` →
  `ShellAction::AiStreamAllowed` → `spawn_ai_request` starts a **detached background thread** →
  events land in the channel → `check_ai_stream` drains them with `try_recv` **on every
  `update()` frame**. That drain is **unconditional** (no `is_ai_streaming` gate) so a `/run`
  log sent from a background thread is not stuck until the next AI request; `is_ai_streaming`
  only decides whether a new prompt is accepted. The worker thread never touches
  `ai_text_area`; output appears only because `update()` keeps looping. If AI output seems
  stuck, look at `check_ai_stream` / `is_ai_streaming`, not at the HTTP thread.
- egui is **repaint-on-demand**, and every `ctx.request_repaint()` in `app.rs` lives inside a
  window that is closed by default — so nothing repaints while idle. `update()` therefore
  calls `ctx.request_repaint()` whenever `is_ai_streaming || shell.bg_tasks > 0`. Drop that
  guard and AI streaming plus `/run` output stall until the next mouse move.
- Responses are **SSE-streamed**: the body carries `"stream": true`, and `stream_sse`
  (called from `spawn_ai_request`) walks `data:` lines off `Body::into_reader()`
  (ureq `BodyReader: io::Read`), accumulating `choices[0].delta.content` until `data: [DONE]`.
  Verified shape on Ollama: `data: {json}` separated by blank lines, terminator `data: [DONE]`.
- Each change emits `AiEvent::Delta(preview)` where `preview` is the **full** current text, not
  an increment — so the UI rewrites its region from scratch every time and a lost chunk costs
  nothing. `extract_preview` (`ai_core.rs`) cannot use `serde_json`: the object only becomes
  parseable once closed, and waiting for that is exactly what streaming must avoid. It byte-scans
  for `"text"`, skips whitespace/colon, then walks to the first unescaped `"`, and finally
  unescapes via `serde_json::from_str::<String>(&format!("\"{}\"", slice))`. While the model is
  still emitting the JSON header (~first 15 of ~37 chunks) the preview is empty and nothing
  appears — that is expected, not a hang.
- `app.rs` holds `ai_stream_marker: Option<usize>` = `ai_text_area.len()` captured on the first
  `Delta`. Every `Delta` truncates back to it and rewrites `"V: " + preview`; `ExecuteAction` and
  `Error` truncate to it before pushing the final output. The `V: ` is **duplicated on purpose** —
  it mirrors `src/core/shell.rs:229`'s `format!("V: {}\n", text)`, otherwise the line jumps 3 chars right
  at completion. Removing the marker makes the chat show the preview *and* the answer stacked.
- `is_ai_streaming` is cleared only by `AiEvent::Finished` or `AiEvent::Error`. If it never
  clears, every later prompt is rejected with "Предыдущий нейро-поток еще активен" until restart.
  `spawn_ai_request` therefore builds an `Agent` with `timeout_global(120s)` — ureq's default
  timeouts are **all `None`**, so dropping that line reintroduces the permanent lockout. Do not
  inflate this value casually: while it runs, `is_ai_streaming` holds the terminal shut.
- `DronePersonality` is **hardcoded inline** in `send_ai_query` (loyalty/irritability/energy as
  constants). There is no personality field on the app; telemetry is not fed into it. The large
  commented-out block just above those constants references a nonexistent `self.personality` and
  `DronePersonality::new()` — **do not uncomment it**, it does not compile.
- `AiEvent::ExecuteAction` → `shell.execute_ai_action(action)`; the returned `String` (reply text
  for `TextResponse`, the "created script, run with `/run <id>`" log otherwise) **must** be pushed
  into `ai_text_area`. Discarding it hid the AI's answers entirely and stopped
  `save_code_if_exists` from ever seeing generated code, since it scans `ai_text_area`.
  - **One branch does not run locally.** If the action is `CreateScript` with `language == "rust"`,
    `app.rs::check_ai_stream` does *not* call `execute_ai_action`. It prints a placeholder, spawns
    a thread that sends `Request::CompileRust` to the core, and the reply comes back later as
    `AiEvent::ActionOutput` (see the IPC section below). `execute_ai_action`'s own `rust` arm is
    now only a fallback for when the core is unreachable — keep the two in sync.

## Module map

Layout follows the process split — engine in the lib, GUI in the shell binary:

- `src/lib.rs` — `vanguard_os_lib`. Declares the four public modules below. Both binaries
  depend on it; that is the only way they share types.
- `src/ipc.rs` — the IPC protocol and nothing else: `Request`/`Response`/`Event` enums
  (serde, `tag = "cmd"`/`"kind"`, `snake_case`) plus `CORE_PORT` (7331) and `core_addr()`.
  Single source of truth for the wire format. Add a request here first, then a handler in
  `bin/core.rs`, then a caller. The `Fs*` requests (file explorer) also carry their payload
  types here (`FsEntry`/`FsListing`/`FsFile`) — see the file-explorer section below.
- `src/ai_core.rs` — the AI request, split into three: `build_request` (env config, message
  assembly, body — **pure, no I/O**, returns `Err(String)` instead of sending events),
  `spawn_ai_request` (thread, agent, POST, non-2xx check) and `stream_sse` (the SSE reader),
  plus `extract_preview` and the `VanguardAction` JSON protocol. Non-2xx does not reach here as
  an error (the agent is built with `http_status_as_error(false)`), the status is checked
  before parsing. Both error paths now `return Err` and the caller sends `AiEvent::Error` —
  **neither sends `Finished`**, which is exactly what they did when they sent the event
  themselves; do not "fix" that into a `Finished`. Lives in the lib because the future
  `vanguard_ai` assistant will reuse it; the shell calls it over an in-process channel, not IPC.
- `src/core/shell.rs` — `VanguardShell::handle_command` parses `/…` and returns a `ShellAction`;
  `execute_ai_action` carries out AI-requested work. Still called from the shell's
  `check_ai_stream`, but its `/run` and `rustc` paths are now also reachable through the core
  (see `bin/core.rs`).
- `src/core/executor.rs` — runs `vanguard_gen_<id>.py` through `python` (`python3` on
  non-Windows). Used by both processes.
- `src/core/telemetry.rs` — background thread sampling CPU/RAM/disk into
  `Arc<Mutex<SystemMetrics>>` every 500 ms; the UI reads that guard, never calls sysinfo
  directly. Each process spawns its own daemon — the core's is for `/hardware`, the shell's
  for the desktop graphs.
- `src/bin/core.rs` — `vanguard_core` main: binds `127.0.0.1:7331`, one thread per connection,
  each with its own `VanguardShell` (independent dialogue history). Maps `Request` → work:
  `Shell` → `dispatch_shell`, `RunScript`/`CompileRust` → background thread streaming
  `Event::TaskLog` back over the same socket, the rest synchronously — including all seven `Fs*`
  handlers, which run inside `resolve_path()` (see the file-explorer section). `send()` swallows write
  errors on purpose — a client that disconnected mid-stream must not panic the core.
- `src/main.rs` — `vanguard_os` main: spawns the shell's telemetry daemon, then
  `eframe::run_native`. `#![windows_subsystem = "windows"]`, prints nothing to stdout.
- `src/app.rs` — `VanguardOSApp` implements `eframe::App`; owns boot stages, all windows
  (grouped in `VanguardWindows`), the AI stream, the IPC client calls, and the command input
  entrypoint (`send_ai_query`). The desktop itself was extracted to `desktop.rs`; `app.rs` only
  builds a `DesktopHost` each frame and executes the returned `DesktopIntent`s via
  `apply_desktop_intent`. It still owns the `fs: FsExplorer` field; every line behind that
  field lives in the child module right below.
- `src/app/fs_explorer.rs` — the "Файлы" window: `FsExplorer`, the `FsKind`/`FsReply` pair that
  tells the background thread what to parse, the `fs_*` IPC helpers and `draw_fs_window`, itself
  split into five free functions (`fs_row_path`, `fs_row_actions`, `fs_panes`, `fs_row_status`;
  the two panes live inside `fs_panes`) over two shared ones (`fs_named_input`, `fs_btn_create`).
  There is no `fs_apply` any more: **no `Fs*` call runs on the UI thread**, so the old
  `match fs_call { Ok => reload, Err => status }` became `fs_spawn` + `fs_drain`.
  - **It is a child module of `app`, not a sibling:** `mod fs_explorer;` is declared *inside*
    `app.rs`, which resolves to `src/app/fs_explorer.rs`. Private fields of `VanguardOSApp` are
    visible only in the declaring module **and its descendants**, so a sibling registered in
    `main.rs` would need `pub(crate)` on every field it touches.
  - The helpers stay **free functions taking `&mut FsExplorer`**, not methods: the window calls
    them from inside `VanguardWindow::show()`, where `&mut self` is already taken by the window
    itself (same trick as `walk_history`).
- `src/ipc_client.rs` — the shell's side of IPC: `call()` connects, autostarts `vanguard_core.exe`
  if nothing is listening (3 s of retries, spawned with `CREATE_NO_WINDOW`), sends one request,
  and reads responses **until a final `Ok`/`Err`/`Pong`** — see the warning below.
- `src/desktop.rs` — the whole desktop surface: wallpaper, icon grid, context menu, taskbar,
  Start menu and the clock/calendar popup. It **owns no windows, shell or chat**: it reaches
  them through `DesktopHost` (a struct of borrows for one frame) and returns
  `Vec<DesktopIntent>` for `app.rs` to run. Its geometry is drawn against the real viewport
  (see "Desktop layout and widgets" below), never against a constant.
- `src/ui_window.rs` — thin `VanguardWindow` wrapper over `egui::Window`; carries `minimized`,
  which egui has no native concept of (`show()` just returns early). Also holds
  `WindowId` (the window enum) and `VanguardWindows`, which groups all ten windows so the
  taskbar can enumerate them via `WindowId::ALL`. Adding a window touches **seven** places:
  the enum, `ALL`, `glyph()`, `label()`, the struct, `new()`, `get()` and `get_mut()` — the
  last two are match arms, so a missed one is a compile error, not a silent ghost.
- `trainer/vanguard_train.py` — QLoRA self-finetune on the app's own dialogues; launched by
  the Neural Trainer window (`app.rs::start_trainer`) through `python`, logs stream to the GUI
  chat line by line exactly like `/run` does.
- `*.rs.bak` are generated artifacts, not source (see below).

## IPC — the one rule that is easy to get wrong

`ipc_client::call` must stop reading at the **final** response (`Ok`/`Err`/`Pong`). The core
serves each connection in a loop waiting for the *next* request, so the socket stays open; if
the client read to EOF it would block until the 30 s read timeout on every call. Events
(`Event::TaskLog`) always precede the final `Ok` for their request — one `/run` = N events
then `Ok("<< TASK DONE >>")`. `saw_final` + `break` in the match arms is what makes this work;
do not "clean it up" into a plain `for` loop.

Two more invariants:

- **The file explorer's IPC is background too — and drained unconditionally.** `fs_spawn`
  (`app/fs_explorer.rs`) `fetch_add`s on the same `bg_tasks`, hands the reply to a channel, and
  `VanguardOSApp::fs_drain` reads it back `fetch_sub`ing afterwards. `update()` calls `fs_drain()`
  right after `check_ai_stream()` with no gate: `busy`/`pending` do the gating, not the frame.
  Moving the drain into `draw_fs_window` would strand a reply if the window closed mid-flight;
  decrementing before the read would let egui sleep on an unread result. It also means opening
  «Файлы» no longer freezes the window for the ~3 s an uncached `C:\` listing used to cost.

- **Heavy work is async and holds `bg_tasks`.** `app.rs::try_core_run` and the AI's Rust-compile
  path both `fetch_add` on `shell.bg_tasks` before spawning the IPC thread and `fetch_sub`
  after sending the reply. `update()`'s repaint guard reads that counter, so dropping it stalls
  `/run` output and compile results until the next mouse move — the same failure mode as the
  `is_ai_streaming` guard above.
- **A reply can arrive after a new stream started.** `AiEvent::ActionOutput` (compile result)
  is inserted *at* `ai_stream_marker`, not appended, and the marker is shifted by the inserted
  length — otherwise the next `Delta` truncation would delete the result. It also does not go
  into dialogue history: `ExecuteAction` already pushed a placeholder when it dispatched the
  compile.

## Desktop layout and widgets

Geometry is drawn against the **real viewport**, read every frame at the top of
`app.rs::update()` from `ctx.screen_rect()` — not from a constant. That is the whole reason
the desktop survives maximize, resize and F11: the taskbar, Start menu and clock popup all
take `DesktopHost::win_w`/`win_h`, and that pair *is* this rect. 1024×768 stays the
**reference size** (`app::BASE_W`/`BASE_H`, the same numbers `with_inner_size` gets in
`main.rs`); the boot logo center is scaled proportionally from `BASE_CX`/`BASE_CY` (512/284),
so at the reference size nothing moved when this became dynamic. **A regression check is
therefore still 1024×768**: if the taskbar or the boot screen shifts at that size, the
scaling drifted.

What reads which size:

| element | source |
|---|---|
| wallpaper, grid, watermark | `ui.max_rect()` in `draw_wallpaper` |
| icon grid (number of rows) | `ui.max_rect().height()` in `draw_icons` |
| taskbar, Start menu, clock popup | `DesktopHost::win_w` / `win_h` |
| maximized `VanguardWindow` | `ctx.screen_rect()` in `ui_window.rs::show` |
| first-open position of a window | `default_pos` scaled from the 1024×768 reference, then clamped inside the screen |
| boot logo / progress bar | `cx`/`cy` = viewport × (512/1024, 284/768) |

F11 toggles borderless fullscreen via `ViewportCommand::Fullscreen(bool)`. The flag is read
back from `i.viewport().fullscreen`, **never kept in app state** — Windows can leave fullscreen
on its own and a local flag would then lie. The window is resizable with
`min_inner_size = 1024×768`, so it cannot be shrunk below the size every constant above was
measured against.

`DesktopIcon` carries no `x`/`y` at all: `default_icons()` declares only `glyph`/`label`/
`hint`/`action`, and `draw_icons` places them by index. Adding an app means adding a
`WindowId`, one entry in `WindowId::ALL`, one `DesktopIcon` — and nothing else.

Widgets and features added on top of the original desktop:

- **Backend status** (taskbar, right, a single tray dot before the clock): green = Ollama answers,
  red = nothing on the endpoint. The flag is an `Arc<AtomicBool>` written by a **detached probe
  thread** started in `VanguardOSApp::new`, which does a TCP connect to the endpoint every 2 s.
  Probing inside `update()` would stall the frame on DNS + connect — do not "simplify" it away.
  The probe target comes from `ai_core::configured_url()`, which is also what
  `spawn_ai_request` uses; go through it instead of re-reading env, or the dot and the actual
  request can drift apart. `app.rs` reads the `AtomicBool` once per frame into
  `DesktopHost::backend_online`; the desktop never touches atomics itself. The full
  `model @ host:port` string lives in the Settings window, not on the taskbar.
- **Taskbar** (`desktop.rs::draw_taskbar`): one pinned-style button for **every** window in
  `WindowId::ALL` (open or not — closed ones show no status pill), plus Start, clock, tray dots and
  the show-desktop strip; right side shows `bg_tasks` and the streaming state. It enumerates windows
  through `WindowId::ALL` in `ui_window.rs` — **add a window there and to the `VanguardWindows`
  struct together**, otherwise it silently never appears. Clicks come back as
  `DesktopIntent::ToggleWindowMinimized` and are applied in `app.rs::apply_desktop_intent`, which is
  **also where a closed window gets opened** (Windows-11 behaviour): the intent only says "toggle",
  so if `is_open` were not checked there, clicking any of the nine closed apps would silently flip
  `minimized` and do nothing — that bug shipped once already. A freshly opened Terminal additionally
  gets `focus_input_pending`.
- **Context menu** (right-click anywhere on the desktop): opens by storing the cursor position in
  `Desktop::menu_pos`; closes on a menu item, on Esc, or on `Response::clicked_elsewhere()`, which
  reacts to **primary clicks only** — a right-click relocates the menu instead of closing it.
  Menu items emit `DesktopIntent`s (focus input, open explorer, clear chat, `/hardware`, `/help`)
  rather than calling the shell directly, so the desktop stays free of `&mut self` on the app.
- **Hotkeys** (`desktop.rs::handle_hotkeys`): `F1` → `/help`, `Ctrl+L` → clear chat, `Enter` →
  focus the input line. The Enter handler is gated on `!ctx.wants_keyboard_input()`, otherwise it
  would swallow the Enter that submits a typed command. They are checked in `update()` *before*
  the windows are drawn — moving that check later changes which widget sees the key first.
- **Script Explorer** (`self.windows.scripts`): lists `vanguard_gen_*.py` and shows the selected
  file's source. RUN goes through `explorer_run` executed *after* `VanguardWindow::show` returns —
  `run_shell_command` borrows all of `self`, which would collide with the window's own
  `&mut self.windows.scripts`. Its status line lives in `explorer_status`, **not** in
  `ai_text_area`: writing to the chat from a window while a stream is in flight gets clobbered
  by the marker rewrite. The explorer's own state (`explorer_sel`/`explorer_code`/
  `explorer_status`) still lives on `VanguardOSApp`, not on `Desktop` — it is window state,
  not desktop surface.
- **File Explorer** (`self.windows.explorer`, «Файлы»): a real directory tree served by the core
  over IPC — **the shell never touches the filesystem here** (no `std::fs` anywhere in
  `draw_fs_window`) — the whole feature now lives in the child module `src/app/fs_explorer.rs`.
  Its state is one `FsExplorer` struct on `VanguardOSApp`, and its IPC helpers
  `fs_call`/`fs_reload`/`fs_leave`/`fs_up`/`fs_enter`/`fs_open_file`/`fs_save` are **free functions,
  not methods** — the same trick as `walk_history`: they run inside `VanguardWindow::show`'s
  closure, where `&mut self` is already taken by the window itself, and only `self.fs` is
  additionally captured (disjoint field capture, edition 2021). Requests are issued **inside** the
  closure for the same reason. List/read/write happen **only on a click**, never per frame, or every
  frame would pay for a TCP round trip. First open (and every re-open, tracked by `fs.loaded`,
  reset while the window is closed) issues exactly one `FsList`.

## Runtime file side effects — run the app from the repo root

Everything the app writes or globs uses a **relative path against the process CWD** — and after
the split there are **two processes with two CWDs**:

- **The shell** (`vanguard_os`) writes `vanguard_notes.txt`, and globs `vanguard_gen_*.py` for
  `/apps`, the LAUNCHER and the Script Explorer.
- **The core** (`vanguard_core`) is what actually runs `/run` and compiles AI Rust. When the
  shell **spawns** it via `ipc_client::spawn_core_process`, the child inherits the shell's CWD,
  so both see the same files. But if you start the core **yourself** from another directory
  (`cargo run --bin vanguard_core` from `src/`, say), its CWD differs and `/run` will report
  "file not found" for scripts the shell can clearly see. **Start the core from the repo root**
  (or let the shell spawn it) — that is the whole reason `cargo run` alone is enough.
  The file explorer no longer depends on the CWD at all (it now spans every drive), so a core
  started from `src/` lists the same disks as one started from the repo root.

Generated artifacts, all relative to the *core's* CWD: `vanguard_gen_<id>.py`,
`vanguard_tmp_<id>.rs`, `vanguard_app_<id>.exe`, `vanguard_script_<id>.bat|.sh`.

- Notes autosave: the Notes window writes `vanguard_notes.txt` whenever the text changes
  (`app.rs::update`, gated on `notes_dirty`) and reloads it at startup. The save is
  **not** tied to shutdown — `/exit` is `process::exit(0)`, so `eframe::on_exit` never runs and
  anything waiting for it would be lost. A failed save is reported in the GUI chat, not the
  console.
- `/apps` and the LAUNCHER window `glob("vanguard_gen_*.py")` to list apps; `/run <id>` executes
  one. No such file is tracked by git and none exist in a fresh checkout, so `/apps` prints
  "REGISTRY EMPTY" until something is generated — expected, not a bug. `/wipe` deletes every
  matching file, so it only ever destroys untracked, locally generated scripts.
- **Dialogue journal** `vanguard_dialogues.jsonl`: every `push_user`/`push_assistant` appends one
  JSON line (role/content/ts) via `src/core/shell.rs::log_dialogue_line` — that is the training dataset for
  the Neural Trainer. It is personal correspondence and stays git-ignored on purpose.
- The AI Python-code extractor in `src/core/shell.rs` only matches code fences tagged `python`.

`.gitignore` covers all of the above (`vanguard_gen_*`, `vanguard_app_*`, `vanguard_script_*`,
`vanguard_tmp_*`, `vanguard_notes.txt`, `*.rs.bak`), so a clean checkout stays clean. The older
note that "`.gitignore` contains only `/target`" is long obsolete — do not trust it.

External tool requirements for those paths: `python` on PATH for `/run`, `rustc` on PATH for
AI-generated Rust (the **core** compiles with `rustc -O` — see `src/bin/core.rs::compile_rust`;
the shell's local `execute_ai_action` still has a `rustc` branch as a fallback when the core is
down, and both must stay in sync if you change compile flags).

Slash-commands: `/help /apps /run <id> /hardware /wipe /reset /exit`. Anything else that does not
start with `/` is sent to the LLM.

## File explorer «Файлы» — all drives, sandbox removed (added 2026-10-14, jail cut 2026-10-18)

Seven requests in `src/ipc.rs` (`FsList`/`FsRead`/`FsWrite`/`FsCreate`/`FsMkdir`/`FsRename`/`FsDelete`)
and their handlers in `src/bin/core.rs`. Every path a client sends is now **absolute**
(`C:\Users\...`), and an empty `FsList.path` means the virtual level «Этот компьютер», where the
core answers with the drive letters it finds (`C:\`, `D:\` …). Every path still goes through
`core.rs::resolve_path()` before it may touch disk.

The old jail (`sandbox_path(root, rel)` against `canonicalize(current_dir())`) was removed on
purpose: with the project directory as the root, «Файлы» showed a folder instead of a computer.
What is left below is not a boundary any more — it is correctness, plus two guards against the
worst one-click accidents.

The shell side of the feature is background now: `fs_spawn` puts every `Fs*` call on its own
thread and `VanguardOSApp::fs_drain` reads the reply back — see the `bg_tasks` invariant in the
IPC section above.

| rule | why it exists |
|---|---|
| `..` is rejected **as text, before** `canonicalize()` | canonicalization walks up from the nearest existing prefix, so a `..` peeled off along the way would survive into the result as plain text and the resulting path would not exist |
| the path must be absolute | `C:file` on Windows means "relative to the current directory of drive C", so two cores started from different CWDs would answer about different files — the same trap `/run` already documents |
| the longest existing prefix is canonicalized, missing components re-appended | `canonicalize()` needs an existing path; `mkdir -p` and "new file" create two new levels at once. A not-yet-existing component cannot be a symlink, so trusting it is safe |
| the **drive root** may be listed but not deleted or renamed | `Path::parent()` is `None` there; `remove_dir_all("C:\")` is one click away in the list and would stop only on a couple of protected system files |
| `A:` and `B:` are skipped when enumerating drives | on a machine without a floppy `GetFileAttributes` on those letters answers slowly and for nothing |
| files over 1 MiB and non-UTF-8 files are refused by `FsRead` | the shell keeps the text in memory and repaints it every frame |
| create and rename **refuse an occupied name** instead of replacing it | `fs::write` truncates an existing file and `fs::rename` replaces the target silently (std docs: "replacing the original file if `to` already exists"; on Win10 1607+ it matches Unix), so «＋ Файл» used to zero a file whose name was taken and rename used to destroy the victim. `FsCreate` uses `create_new(true)` — an atomic `AlreadyExists` — and `fs_rename` checks `dst.exists()`; `src == dst` is a deliberate no-op, not "occupied". `FsWrite` itself **still overwrites**: it is the SAVE path for the file already open in the editor |
| the GUI's `✕ Удалить` is **two clicks** | the jail is gone, so a single stray click on a selected row could remove a whole tree. The first click only arms `FsExplorer::confirm` and relabels the button; changing the selection disarms it. This is the GUI's rule, not the core's — `FsDelete` itself will happily delete any non-root path |

`fs_pretty()` strips Windows' `\\?\` verbatim prefix for **display only** (the path bar would
otherwise read `\\?\C:\Users\...`); never compare against the pretty string.

Payload types (`FsEntry`/`FsListing`/`FsFile`) live in `ipc.rs`, not in `core.rs`, because the
shell deserializes them in `app.rs` — putting them anywhere else would break the "one source of
truth" rule the same file already states for the wire format. Note `FsListing.entries` arrives
**pre-sorted** (directories first, then case-insensitive name); the GUI must not re-sort.

Two wire-level details worth not "cleaning up":

- `FsEntry.mtime` is `#[serde(default)]`. It was added after the other fields, and
  `vanguard_core.exe` **outlives** the shell — a not-yet-restarted old core simply omits it, and
  a hard requirement would break parsing of the whole listing instead of just one column.
- `FsListing.path` is the core's **canonical** answer and `fs_apply_reply` overwrites the GUI's
  own `fs.path` with it. That is why the breadcrumb bar always names what was really listed
  (register, junctions and doubled slashes get normalised there), and why a failed navigation
  leaves the previous path in place rather than a path nobody reached. The GUI never sets
  `fs.path` at click time for the same reason.

Smoke test: `%TEMP%\opencode\fs_smoke.ps1` (ASCII-only — PS 5.1 chokes on Cyrillic in a `.ps1`
here) starts a fresh `vanguard_core.exe` over 8.3 short paths, drives the six original requests
over raw TCP and parses both JSON levels of `Response::Ok`. It asserts behaviour, not message
text — the error strings are Russian, so substring matching on them is unreliable. **46 checks,
last run 2026-10-19, all passing** — virtual root listing drives, `mtime` carried, `..`/relative/
blank rejected, drive-root delete and rename refused, `create_new` occupied-name guard, rename
refusing an occupied target with the victim intact, `FsWrite` still overwriting on the SAVE path,
and no scratch files left behind. Two PS 5.1 traps it encodes: every `.Count` goes through `@()`
(a *single* object reports `$null`, not `1`), and the path used for assertions is taken from the
core's own reply, because `$env:TEMP` is an **8.3 short name** in this environment
(`C:\Users\73B5~1\...`) while the core echoes the canonical long one.

## Self-finetune loop — Neural Trainer window (added 2026-10-12)

The app can fine-tune its own model on its own dialogues. The honest loop (not model collapse):

| piece | where | behavior |
|---|---|---|
| dataset | `src/core/shell.rs::log_dialogue_line` | appends every user/assistant reply to `vanguard_dialogues.jsonl`; `count_dialogue_lines()` feeds the window's counter |
| window | `WindowId::Trainer`, `app.rs::draw_trainer_window` | editable HF model id, dialogue count, start button; refuses while `is_ai_streaming` (training unloads Ollama out of VRAM) |
| runner | `app.rs::start_trainer` | spawns `python trainer/vanguard_train.py --hf-model <HF>` on a detached thread; stdout → `AiEvent::Token` with `T: ` prefix, holds `bg_tasks`, guard flag `trainer_running` |
| training | `trainer/vanguard_train.py` | 4-bit NF4 + LoRA r=16, loss masked to the answer only, **holdout = last 10% of pairs** printed at the end; adapter saved to `trainer/runs/lora_<time>/` (git-ignored), base model never modified; refuses below 10 pairs |

Traps that will bite you:

- **`pip install torch` from PyPI is CPU-only on Windows** — QLoRA then refuses to start. Install
  `pip install --index-url https://download.pytorch.org/whl/cu126 --force-reinstall torch`
  (driver 560.94 = CUDA 12.6; `torch.cuda.is_available()` must print True).
- **HF model id ≠ Ollama tag**: the window defaults to `Qwen/Qwen3.5-9B` (env override
  `VANGUARD_TRAIN_HF_MODEL`). A 9B 4-bit load is ~5.6 GB + LoRA ≈ 8 GB — that only fits because
  the script first calls Ollama `/api/generate keep_alive:0` to evict the resident model. Ollama
  answering AI queries during training will fail with a VRAM error — expected.
- **Guard the holdout loss**: `summary.json` next to the adapter carries train/holdout loss. If
  holdout did not improve vs the baseline, the adapter learned noise — do not merge it into GGUF.
- A 9B HF download is ~18 GB the first time; the smoke test
  `--hf-model Qwen/Qwen2.5-0.5B-Instruct --steps 3` runs in minutes and validates the whole chain
  (verified end-to-end 2026-10-12: LoRA attached 8.8M/502M params, 3 steps ≈ 3.5 s, adapter +
  `summary.json` written, holdout loss computed).
- transformers 5.19 **removed `warmup_ratio`** from `TrainingArguments` — only `warmup_steps`
  exists; `torch_dtype` is deprecated in favor of `dtype`. The script uses the new names.

## `src/optimize_project.py` — AI refactoring, now compile-gated (rewritten 2026-10-11)

The old script POSTed each `src/*.rs` file to local Ollama (`localhost:11434`, `qwen2.5-coder:32b`)
and **overwrote the source in place** with unverified output — that is the origin of `src/*.rs.bak`
(.bak copies exist only for `main.rs`, `executor.rs`, `telemetry.rs`; there is no `app.rs.bak`, and
none of them is a real safety net). The rewrite keeps the idea and removes the hazard:

| aspect | behavior |
|---|---|
| model/endpoint | same `VANGUARD_AI_*` env as the app; default `qwen3.5:9b`, `reasoning_effort: none` (thinking off); `--model/--url/--think` override |
| write gate | the candidate goes through `cargo check`; on failure the source is restored from a backup in a temp dir — **no `.bak` files appear in the repo anymore** |
| `--dry-run` | prints a unified diff to stdout, touches nothing |
| tasks | `--task cleanup|unwrap|docs`: narrow mechanical rewrites, not "optimize memory and algorithms" |
| throughput | prints measured tokens/s per file; warns when the model string says >11 GB params (32b = ~6 tok/s against the 12 GB 3080 Ti) |
| scope | positional file args, default every `src/*.rs`; paths resolve against the repo root, so any cwd works |

Verified end-to-end 2026-10-11 with the daemon up: `--task docs --dry-run src/telemetry.rs`
produced a docs-only diff (978 tokens / 20 s) with clean fence extraction; with the daemon down
the target file stayed byte-identical.

`cargo check` proves compilation only — the script says so in its own output. There are no tests
here to catch a behavior change, so review `git diff` before committing anything it applied.

## AI backend — local Ollama, configured via env

`spawn_ai_request` (`ai_core.rs`) reads everything from the environment; there is no hardcoded
URL or key anywhere:

| Var | Default | Meaning |
|---|---|---|
| `VANGUARD_AI_URL` | `http://localhost:11434/v1/chat/completions` | OpenAI-compatible chat endpoint |
| `VANGUARD_AI_MODEL` | `qwen3.5:9b` | any Ollama tag; the default is a **hybrid** model, see below |
| `VANGUARD_AI_KEY` | `ollama` | sent as `Bearer`; Ollama ignores it |
| `VANGUARD_AI_KEEP_ALIVE` | `30m` | passed as `keep_alive`; `-1` = never unload. **Numeric values must be sent as a JSON number**: Ollama 0.40 rejects the *string* `"-1"` with 400 `time: missing unit in duration`, so `ai_core.rs` parses the env value as `i64` first and only falls back to a duration string (`30m`) when that fails |
| `VANGUARD_AI_REASONING_EFFORT` | `none` | copied into the request body as `reasoning_effort`. **`none` = thinking off** (0.99 s mean); unset = field omitted, thinking runs free (27 s mean). Other values are not a middle ground — see the benchmark table below. |

The old `https://deepseek.com` + `sk-vanguard-key` placeholder is gone — that URL is the site's
front page, so its WAF answered 403 before any API logic ran. The startup log now prints
`>> CORE LNK: endpoint=… model=…` so a bad config is visible immediately.

### Speed is decided by VRAM, not by the Rust client

`nvidia-smi`: **RTX 3080 Ti = 12288 MiB**. A model heavier than that gets split onto the CPU and
generation collapses. Measured with `POST /api/chat` (it reports `load_duration` /
`prompt_eval_duration` / `eval_duration`), then cross-checked end-to-end on the same 3-prompt
run through `/v1/chat/completions`:

| | `qwen2.5-coder:32b` (21 GB) | `qwen2.5-coder:7b` (4.4 GB) |
|---|---|---|
| offload | `50%/50% CPU/GPU` | **100% GPU** (`size_vram == size`) |
| generation | **≈6 tok/s** | **≈136 tok/s** |
| same 3 prompts, wall time | 24 / 14.5 / 12.0 s | 0.96 / 0.56 / 0.45 s |
| protocol | 3/3 | 3/3 |

Prefill was never the problem (35 tokens in 86 ms ≈ 405 tok/s) and `load_duration` is ~10 ms
while the model stays resident — model size is the whole story: **keep any model ≤ ~11 GB** or
it silently slides onto the CPU. The Rust side contributes milliseconds (the request already
runs on a detached thread, so nothing blocks the UI, and tokio/async would change no measured
number). A bare `curl.exe` reports `total=0.30-0.54s` for a short reply where PowerShell's
`Invoke-WebRequest` reports 2.5 s for the identical call — never benchmark through it. It also
returns `Content` as **`byte[]`** only when the response lacks `charset=utf-8` (Cyrillic replies
come back as `string`), so decode by type or `ConvertFrom-Json` silently yields an array of .NET
fields instead of the timings.

`OLLAMA_FLASH_ATTENTION=1` is set in the **User** environment scope; it only reaches the daemon
on restart, so a freshly booted shell or a reinstalled Ollama may not have it.

### Hybrid (thinking) models — `reasoning_effort`, not `think`

The default `qwen3.5:9b` (6.6 GB, 100% GPU, same as 7b) is a **hybrid** model: left to itself it
burns its whole budget on reasoning and answers in **18–44 s** instead of ~0.5–1.6 s. Measured on
`/v1/chat/completions` — the endpoint the app actually uses — for the trivial prompt `Say OK`
(`completion_tokens` / wall time):

| request field | completion_tokens | wall |
|---|---|---|
| *(omitted)* | 424 | 4.69 s |
| `"think": false` | 223 | 2.51 s |
| `"enable_thinking": false` | 225 | 2.52 s |
| `"reasoning_effort": "none"` | **2** | **0.33 s** |
| `"reasoning_effort": "medium"` | — | **HTTP 400** |

Two traps here:

- `think` / `enable_thinking` are the knobs that work on Ollama's **native** `POST /api/chat`
  (171 eval tokens for `Say OK` vs 2 with `"think": false`), but on the OpenAI-compatible
  `/v1/chat/completions` they are only *advisory* and still leave ~220 thinking tokens behind.
  Always benchmark on the endpoint the app uses.
- `reasoning_effort` values other than `"none"` are **rejected with HTTP 400**
  `"qwen2.5-coder:7b" does not support thinking` on models that cannot think. `"none"` is
  accepted by every model tested (7b, qwen3.5:9b, deepseek-r1:1.5b all return 200), which is
  why the app sends the field only when `VANGUARD_AI_REASONING_EFFORT` is set and why `"none"`
  is the safe value to put in the environment.

**Current configuration: thinking is OFF.** `VANGUARD_AI_REASONING_EFFORT=none` is set in the
User scope (re-set 2026-10-10 after it had been deleted). Measured on the real production system
prompt — the long one `generate_system_prompt` actually builds, not a shortened stand-in, because
prompt length turns out to dominate the cost:

| `reasoning_effort` | wall (trivial / medium / "write a file") | mean |
|---|---|---|
| *(omitted — full)* | 14.4 / 27.6 / 38.9 s | **27.0 s** |
| `low` | 2.6 / 14.2 / 30.3 s | 15.7 s |
| `medium` | 9.3 / 13.6 / 8.6 s | 10.5 s |
| `none` | 0.71 / 0.93 / 1.32 s | **0.99 s** |

So full thinking is **27× slower** than `none` on the prompt the app really sends — worse than the
old stand suggested, because the Russian persona prompt plus five JSON examples is long enough to
prime a large reasoning budget. `low` and `medium` are not a middle ground: both still cost
10–16 s and vary 12× between prompts (2.6 s vs 30.3 s for `low`), so they give neither
predictability nor speed. Protocol holds **3/3 in every row**, including `none`.

**Do not try to cap thinking with `max_tokens` / `num_predict`.** It was the obvious next idea and
it is a dead end: the cap applies to completion tokens *including* reasoning, so a small cap
truncates the answer before the model ever emits `content`. Measured on `qwen3.5:9b`: at 64, 128
and 256 the returned `content` was **empty** in 8 of 9 runs, and only at 512+ did answers start
surviving — by which point the wall time (3.2 s) is worse than `reasoning_effort=none` (0.51 s) for
the same result. The app sends no cap; keep it that way.

Thinking is safe for the app's SSE path: a streamed response carries `delta.reasoning`,
`delta.content` and `delta.role` as **separate** fields and reasoning never leaks into
`content` — checked on a real 361-chunk stream, whose assembled content was clean
`{ "response": "2 + 2 = 4" }`. The visible cost is that `extract_preview` finds no `"text"` for
the first ~350 chunks, so the chat stays silent while the model thinks: that is the "expected,
not a hang" case, and `is_ai_streaming` holds the input line shut meanwhile.

Three vars are set in the **User** environment scope on this machine: `VANGUARD_AI_MODEL=qwen3.5:9b`,
`VANGUARD_AI_REASONING_EFFORT=none`, and `OLLAMA_FLASH_ATTENTION=1`. Ollama does **not** autostart
here, so the daemon must be launched manually (`ollama serve`) before `cargo run`, and
`curl http://localhost:11434/api/ps` shows what is resident.

`OLLAMA_FLASH_ATTENTION=1` only reaches the daemon **on restart**: a daemon started before the var
was set does not see it. The current daemon was launched from a shell that had the var, so it
should have inherited it — but this is *inference, not measurement*. No API endpoint reports
whether flash attention is active, and no before/after comparison was ever run, so **do not treat
any speed number here as attributable to it**. If you want to settle it, restart the daemon and
compare `eval_duration` for the same prompt against a build with the var removed.

### Conversation history — the request is no longer stateless

`spawn_ai_request` takes a `history: Vec<(String, String)>` argument and builds
`[system, ...history..., user]`. It used to send exactly `[system, user]`, so the model had no
memory of previous turns at all. The `VanguardAI::history` **string** that existed alongside
`messages` was written by `save_code_if_exists` and **read by nobody** — it has been deleted
during a readability cleanup; `messages` is now the single copy of the dialogue.

- The dialogue lives in `VanguardAI::messages` (`src/core/shell.rs`), capped at `MAX_HISTORY_MESSAGES`
  (24 = 12 exchanges). Ten exchanges measurably cost +0.4 s of prefill (309 → 1169 prompt
  tokens), which is why there is a cap instead of "keep everything".
- `app.rs::send_ai_query` calls `push_user` **before** spawning, and
  `check_ai_stream::ExecuteAction` calls `push_assistant` with the text `execute_ai_action`
  returned. Both ends are needed: without the assistant half the history would be a list of
  questions the model never answered.
  - **Exception — AI-requested Rust compilation.** That path is now async (`Request::CompileRust`
    over IPC), so `push_assistant` gets a placeholder acknowledgement ("Компиляция Rust ушла в
    ядро…") at dispatch time, not the real result. The compile result arrives later as
    `AiEvent::ActionOutput` and is **not** pushed into history — otherwise a slow `rustc` would
    land in the dialogue after a *newer* user message and scramble the turn order.
- On `AiEvent::Error` the trailing user message is popped, otherwise the next request would
  show the model a prompt it never got to answer.
- `/reset` and `Ctrl+L` go through `clear_memory`, which now clears `messages` too.

Runtime requirement: the Ollama daemon must actually be running. It is installed at
`%LOCALAPPDATA%\Programs\Ollama\ollama.exe` but does **not** autostart on this box — if nothing
is listening on 11434 the app reports "Сетевой сбой шины ИИ (ureq error)". Start it with
`ollama serve` (or the desktop app) before `cargo run`.

Protocol: `generate_system_prompt` must keep listing **all three** `action_type` values (the
`4. Системная команда` example plus the "no other values" sentence) — verified on the real
prompt, 7b and 32b then both return valid `VanguardAction` JSON 3/3. With the old 3-example
prompt 7b invented `action_type: "SystemCheck"` (32b happened to guess `SystemCommand`) and the
answer fell into the `TextResponse` fallback, dumping raw JSON into the chat. Do not trim those
lines for brevity. `deepseek-r1:1.5b` ignores the protocol entirely and survives only thanks to
that same fallback.

`keep_alive` **is** honoured on `/v1/chat/completions` — the endpoint the app actually uses —
not just on `/api/chat`: a `/v1` request refreshed `/api/ps`'s `expires_at` to +30 min (the
value sent in the body) instead of the 5 min default. `http_status_as_error(false)` is set on
the agent so 4xx/5xx reach the body-reading branch and the provider's real reason is shown
instead of a bare `http status: NNN`.

Measured 2026-10-12 (cold = model evicted after idle, the "first request hangs" complaint):
`/api/chat` trivial prompt, `qwen3.5:9b` — **cold 8.23 s total, of which `load_duration` 6.36 s**
(re-reading 5.6 GB into VRAM), vs **warm 1.14 s total, `load_duration` 0.011 s**. With
`keep_alive:-1` `/api/ps` shows `expires_at` year **2319** and `size_vram == size` — the model
never leaves the GPU, so idle time no longer costs anything on the next query. The only
remaining cold case is the very first request after `ollama serve` starts. Remember the string
trap above: the *string* `"-1"` fails with HTTP 400 before any of this runs.

ureq specifics (verified against the pinned crate source, `ureq` 3.4.2):

- `Cargo.toml` enables `features = ["native-tls"]` **only** — the `json` feature is off, so
  `send_json()` is unavailable (it is gated `#[cfg(feature = "json")]`). The code must serialize
  with `serde_json::to_string` and pass the `String` to `.send(...)`.
- ureq 3.4.2 has `header(key, value)`; there is **no `with_header()`** anywhere in the crate.
  A `with_header(...)` call fails with E0599, which then cascades into bogus
  `E0277: the size for values of type str cannot be known` errors on later `match` arms — one
  broken method call, several confusing errors. `&String` values are fine (`TryFrom<&String>`
  exists), so the fix is the rename alone.
