# AI Companion for "The Filing Desk"

## Overview

Add an AI chat sidebar to The Filing Desk file organizer, powered by Groq's free API. The companion can chat generally, answer questions about scanned files, and suggest organization strategies.

## Architecture

### Frontend (JS — single file `src/index.html`)

- **Right sidebar panel** (~350px wide), toggled via toolbar button
- **Chat UI**: message bubbles, input field, send button
- **Settings modal**: API key input (password field), model selector, clear chat
- **File context injection**: scanned file summary auto-injected as system context

### Backend (Rust — `src-tauri/src/lib.rs`)

- New Tauri command: `groq_chat(messages, api_key, model) -> Result<String, String>`
- Uses `reqwest` crate to POST to `https://api.groq.com/openai/v1/chat/completions`
- No API key stored on disk — only in frontend localStorage

## Data Flow

```
User types message
  → Frontend builds messages array (system + file context + history)
  → invoke('groq_chat', { messages, api_key, model })
  → Rust POSTs to Groq API
  → Response returned
  → Displayed in chat panel
```

## System Prompt

"You are a helpful file organization assistant called 'The Filing Desk Companion'. The user has scanned a folder with [N] files across [M] categories. You can help with: file organization advice, answering questions about their files, general assistance. Be concise and helpful."

## UI Components

1. **Toggle button** — toolbar icon, opens/closes sidebar
2. **Sidebar panel** — chat messages + input at bottom
3. **Settings** — gear icon → modal with API key, model selector, clear chat
4. **Chat styling** — vintage aesthetic (manila/paper colors, serif fonts)
5. **Loading indicator** — animated dots while waiting

## Key Files

| File | Change |
|------|--------|
| `src/index.html` | Sidebar HTML + CSS + JS |
| `src-tauri/Cargo.toml` | Add `reqwest` + `json` feature |
| `src-tauri/src/lib.rs` | Add `groq_chat` command |

## Groq Free Tier

- Model default: `llama-3.3-70b-versatile`
- Rate limited but generous for personal use
- Error handling for rate limits with user-friendly message
