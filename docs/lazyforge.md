# LazyForge

LazyForge is the terminal operator console over a PolyForge evidence ledger. It gives an
operator read-only browsing of every task in the ledger plus the operator-side validation
writes: promote `Verified` tasks to `Validated` with a recorded rationale. Models can never
produce these writes; LazyForge is the human side of the gate.

## Running

```sh
lazyforge [--ledger PATH]
```

The ledger path resolves in this order:

1. the `--ledger PATH` flag,
2. the `PF_LEDGER` environment variable,
3. `.pf/ledger.jsonl` in the working directory.

The status bar shows the resolved ledger path, the first 12 characters of the Merkle chain
head hash, the active filter, the total task count, and the focused pane.

## Key map

| Key | Action |
| --- | --- |
| `k` / `Up` | Move the selection up (clamped at the first task). |
| `j` / `Down` | Move the selection down (clamped at the last visible task). |
| `Enter` | Toggle between the list pane and the detail pane of the selected task. |
| `Tab` | Switch pane. Listed in the keymap overlay as a placeholder, but not wired yet: use `Enter` to switch panes today. |
| `v` | Validate the selected task: opens the single confirmation modal. |
| `A` | Bulk validate every currently visible `Verified` task: opens one batch confirmation modal. |
| `r` | Rationale input: free-form text captured for the next single validation (`Enter` commits, `Esc` discards). |
| `/` | Filter tasks by case-insensitive substring of the task id (`Enter` commits, empty input clears). |
| `?` | Toggle the keymap help overlay. |
| `q` | Quit. |
| `Ctrl-C` | Quit. Works from the normal view and through the help overlay; while a modal or input popup owns the keyboard, `Esc` cancels that popup instead. |
| Mouse wheel | Over the list pane: moves the selection like the arrow keys. Over the detail pane: scrolls the content three rows per notch. |

## Validation semantics

### Single validation (`v`)

The confirmation modal shows exactly what will be written: the task id, the validator
identity (`lazyforge-operator`), the rationale that will be recorded (the committed one, or
the default wording when none was typed), and a line stating that commit and diff are
copied verbatim from the task's latest `Verified` entry. What you see is what gets
recorded. `Enter` executes and appends one `Validation` entry, promoting the task from
`Verified` to `Validated`; `Esc` cancels and writes nothing.

### Bulk validation (`A`)

One batch confirmation lists every currently visible `Verified` task (with a `+N more`
overflow line past the display limit) and uses a single `Enter` to execute the whole batch.
Before each write the engine re-reads the task's latest state: if the latest `Verified`
identity changed between the batch snapshot and the re-read, that task is skipped instead
of being promoted against stale evidence.

After execution a summary toast appears above the status bar:

```
done N, skipped M
```

Skip reasons, per skipped task:

| Reason | Meaning |
| --- | --- |
| `needs tool attestation first` | Latest entry is still `ModelClaimed`. |
| `already validated` | Latest entry is already `Validated`. |
| `refuted - validation rejected` | Latest entry is `Refuted`; validation never applies. |
| `no entries for task` | The ledger holds nothing for the task. |
| `state changed under you` | The `Verified` identity changed between snapshot and re-read. |

## Fail-closed behavior

A corrupt or unreadable ledger renders a full-screen `Ledger error` view. No partial data
is ever shown: the task list, detail pane, and status bar stay hidden until the ledger
reads cleanly again. The same fail-closed rule applies if a ledger read breaks while the
app is running.

## Requirements

- A terminal of at least 80x24. Below that, LazyForge shows a `terminal too small` screen
  instead of a broken layout.
- Built with [ratatui](https://ratatui.rs); renders on any terminal ratatui supports.
- Rust: the crate builds on Rust 1.88 and newer (its own MSRV), while the workspace core
  stays at 1.85.
