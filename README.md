# whisper-notes

Voice dictation CLI that appends transcribed speech to a file.

Built in Rust with native X11 global hotkey support—no npm packages, no Python dependencies.

## Features

- **Global hotkey** (Ctrl+Shift+Space) to toggle listening via X11
- **Append-only** mode—transcriptions are appended to your specified file
- **Voice commands**—say "command enter" to insert a newline
- **Timestamp mode**—generate timestamped transcripts for subtitles/captions
- **Sound feedback**—audio cues when listening starts/stops
- **Phrase filtering**—auto-discards common Whisper hallucinations ("thank you", "thanks", etc.)

## Requirements

- Linux with X11
- [perception-voice](https://github.com/user/perception-voice) server running

## Installation

```bash
# Clone and build
git clone https://github.com/user/whisper-notes.git
cd whisper-notes
cargo install --path .

# Copy sound effects
mkdir -p ~/.cargo/bin/sfx
cp sfx/*.wav ~/.cargo/bin/sfx/
```

## Usage

```bash
# Basic usage - append dictation to a file
whisper-notes notes.txt

# Timestamp mode - for transcripts/subtitles
whisper-notes --ts 200 transcript.txt
```

### Options

| Option | Description |
|--------|-------------|
| `--ts <ms>` | Enable timestamp mode. Prefixes each utterance with elapsed time. `<ms>` is the gap (in milliseconds) before starting a new timestamped line. |

### Controls

- **Ctrl+Shift+Space** — Toggle listening on/off
- **Ctrl+C** — Exit

### Voice Commands

| Say | Result |
|-----|--------|
| "command enter" | Insert newline (`\r\n`) |

## Timestamp Mode

When using `--ts`, output is formatted with SRT-compatible timestamps:

```
[00:00:01,234] Hello this is the first utterance
[00:00:05,678] After a gap this starts a new timestamped line
 continuing on same line if within gap threshold
```

The `<ms>` parameter controls how long a pause must be before starting a new timestamped line. For example, `--ts 200` means a quarter-second pause triggers a new line.

## Configuration

The socket path and other settings are currently hardcoded in `src/main.rs`:

```rust
const SOCKET_PATH: &str = "/workspace/perception-voice/perception.sock";
```

Modify as needed for your setup.
