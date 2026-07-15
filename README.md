# The Filing Desk — Desktop App (Tauri)

A desktop file organizer built with **Tauri v2** (Rust backend + HTML/CSS/JS frontend).
It scans a folder, categorizes and tags files, then **moves them directly into place**
— no scripts, no uploads, everything runs locally.

## Project layout

```
File_Organicer/
├── src/                      # Frontend (your existing HTML/CSS/JS)
│   └── index.html
├── src-tauri/
│   ├── src/
│   │   ├── main.rs           # Thin entry point
│   │   └── lib.rs            # Commands: scan_folder, organize_files
│   ├── capabilities/default.json
│   ├── icons/                # Generated app icons
│   ├── Cargo.toml
│   ├── build.rs
│   └── tauri.conf.json
├── package.json
├── vite.config.js
└── README.md
```

## Prerequisites (Windows — where you build the .exe)

1. **Node.js** ≥ 18 — https://nodejs.org
2. **Rust** (uses the MSVC target by default) — https://rustup.rs
3. **Visual Studio Build Tools** with the *"Desktop development with C++"* workload
   (or the standalone *VS Build Tools*). This provides the MSVC linker Tauri needs.

> On Linux/macOS you can develop too, but you need the platform webkit libraries
> (`webkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `libgtk-3-dev`, `patchelf`).

## Build & run (development)

```bash
npm install
npm run tauri dev      # launches the app with hot-reload
```

## Build for distribution (Windows .exe + installer)

```bash
npm install
npm run tauri build
```

Output lands in `src-tauri/target/release/bundle/`:
- `The Filing Desk_setup.exe` — NSIS installer (recommended for sharing)
- `The Filing Desk.exe` — portable executable

The whole thing is ~8–12 MB — far smaller than an Electron build.

## How it works

1. **Choose Folder** → native dialog (`@tauri-apps/plugin-dialog`).
2. **scan_folder(path)** (Rust) walks the directory and returns file metadata.
3. The frontend categorizes/tags files using the same rules as before.
4. **organize_files(root, moves, dryRun)** (Rust) creates category subfolders
   (`PDFs/`, `Spreadsheets/Finance/`, …) and moves each file in place.

No file ever leaves the machine.

## Categories & tags

Edit the `CATEGORY_RULES` / `KEYWORD_TAGS` arrays in `src/index.html` to adjust
how files are sorted. The Rust side only moves files; all classification lives in
the frontend so it's easy to tweak.
