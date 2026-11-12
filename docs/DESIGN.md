# Quarry design

Dark-first native window, shared visual language with the other ashlar showcase apps.

## Visual language
- Fonts: Inter (UI, 13 px), JetBrains Mono (SQL, cells, 12 px).
- Colors: bg `#0F1115`, panel `#161A20`, raised `#1D222A`, border `#2A303A`, text `#E6E8EB`, muted
  `#9AA3AE`; accent blue `#5B9DFF`; error `#F2555A`, warn `#F2A93B`, ok `#3CC8A0`, info chips `#8FB8FF`.
- 8 px grid, 6 px radius, 28 px rows. Title bar 38, toolbar 44, sidebar 240, status bar 24, agent
  panel 360 (right).
- Connection colors tint the title bar and the status bar's connection chip (prod red, staging warn,
  local ok).

## Screens

### Main window
- **Title bar:** traffic lights, connection name with color chip, read-only badge when the human
  connection is set read-only.
- **Toolbar:** Run (⌘↵), Run all (⇧⌘↵), Cancel (⌘.), Explain (⌘E), format, history, agent panel toggle.
- **Sidebar (240):** connection switcher; schema tree (schemas › tables › columns, with type and key
  icons, estimated row counts); filter field; type-ahead.
- **Center:** tabs (editor tabs and table tabs). Editor on top, result grid below, split draggable.
  Grid: row numbers, sticky header with type under the name, NULL in muted italics, JSON cells
  truncated with an expander, numbers right-aligned.
- **Status bar:** `rows 1,000 of ≥ 10,000,000 · 42 ms · pg@staging · agent: read-only · built with ashlar`.

### Agent panel (right, 360)
- Header "via MCP · Claude", connection state, pause button (stops tool calls).
- Transcript of tool calls: tool name, params folded, result size, time. Charts and selections the
  agent made link to their tab.
- **Approval card** for `db.propose_write`: warn border + lock icon, "Agent wants to change data",
  connection chip, the exact SQL, reason, affected table(s), estimate ("~312 rows"), before/after
  preview for ≤20 rows, buttons Deny (default focus) and Approve (requires a click, or ⌘⌥↵ while the card has focus; ⇧⌘↵ is Run all), note
  "Runs in one transaction · logged in the audit log · undo available".

### Chart panel
Opened by `viz.chart` or the human: chart above or beside the grid, query shown in a collapsible
header, type switcher, "made by agent" chip, export PNG/CSV.

### Connection dialog
Engine segmented (PostgreSQL / SQLite), fields, TLS mode, SSH section (collapsed), color, **Agent
access** section: Off / Read-only (default) / Read-only with role `quarry_agent` (shows the
`CREATE ROLE` + `GRANT SELECT` SQL to run), sensitive column patterns. Test button with result.

## Interactions and keyboard
⌘K palette, ⌘↵ run statement, ⌘T new editor tab, ⌘P open table (⌘O is ashlar's File › Open), ⌘F filter grid, ⌘L agent panel,
⌘⇧A focus approval card, F2 edit cell, ⌘⌥S review and commit pending edits (shows the diff first; ⌘S is ashlar's File › Save), ⌘Z undo
pending edit.

## States
Connecting (skeleton tree), query running (progress + Cancel), streaming (rows count up), error
(inline banner with position in SQL), empty result, disconnected (reconnect button), agent paused,
proposal pending/approved/denied/expired.

## Accessibility
Every control has an automation id and an AccessKit role (grid = table with row/col indexes);
approval card is announced as an alert; Deny is the default action; contrast ≥4.5:1 for text;
reduced motion respected; everything reachable by keyboard.

## Mockups
Design canvas (private until shared): https://claude.ai/artifact/AKHpJEkXbN6Dwq9euri6yA — main window, agent write approval, agent-made chart, new connection.
