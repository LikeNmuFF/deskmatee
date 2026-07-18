# Contributing to Deskmatee

Thanks for your interest in contributing. Deskmatee is a local-first, privacy-focused file organizer built with Tauri v2. Every contribution — code, docs, bug reports, feature ideas — helps make it better.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [How to Contribute](#how-to-contribute)
  - [Report a Bug](#report-a-bug)
  - [Suggest a Feature](#suggest-a-feature)
  - [Submit a Pull Request](#submit-a-pull-request)
- [Development Setup](#development-setup)
- [Project Structure](#project-structure)
- [Code Style](#code-style)
- [License](#license)

## Code of Conduct

This project is governed by the [Contributor Covenant](CODE_OF_CONDUCT.md). All participants are expected to uphold its standards.

## How to Contribute

### Report a Bug

Open a [bug report](.github/ISSUE_TEMPLATE/bug_report.md) and include:

- steps to reproduce
- what you expected vs what happened
- OS and app version (from the beta tag in the top bar)
- screenshots if applicable

### Suggest a Feature

Open a [feature request](.github/ISSUE_TEMPLATE/feature_request.md) and include:

- what problem you're trying to solve
- how you imagine it working
- any alternatives you've considered

### Submit a Pull Request

1. Fork the repo and create a branch from `main`.
2. Make your changes following the code style below.
3. Test your changes — build the app and verify the affected feature works.
4. Open a PR using the [pull request template](.github/PULL_REQUEST_TEMPLATE.md).
5. Keep PRs focused. One feature or fix per PR.

## Development Setup

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable)
- [Node.js](https://nodejs.org/) (v18 or later)
- [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) (platform-specific)

### Getting started

```bash
git clone https://github.com/LikeNmuFF/deskmatee.git
cd deskmatee
npm install
npm run tauri dev
```

This starts the Vite dev server with hot-reload and launches the Tauri desktop window.

### Build for production

```bash
npm run tauri build
```

## Project Structure

```
deskmatee/
├── src/              # Frontend (HTML, CSS, JavaScript)
│   └── index.html    # Main app UI (~1650 lines)
├── src-tauri/        # Rust backend
│   ├── src/
│   │   └── lib.rs    # All Rust logic (commands, server, AI)
│   ├── Cargo.toml
│   └── tauri.conf.json
├── index.html        # Marketing landing page
├── file-organizer.html  # Browser-only prototype
├── package.json
└── vite.config.js
```

## Code Style

- **JavaScript**: vanilla JS, no framework. Follow the existing patterns (camelCase, template literals, arrow functions, no semicolons).
- **Rust**: standard `cargo fmt` formatting, 4-space indent, snake_case for functions and variables.
- **CSS**: custom properties for colors, no preprocessor. Keep selectors flat.
- **No new dependencies** unless there's a strong reason. Prefer simple, self-contained solutions.

## Privacy

Deskmatee runs entirely locally. No data is ever sent to any server unless the user explicitly provides an API key for the AI companion feature. Contributions should not introduce telemetry, analytics, or external network calls without user consent.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENCE.md).
