//! whisper-notes - Voice dictation CLI that appends to a file
//!
//! Usage: whisper-notes <file>
//!
//! Press Ctrl+Shift+Space to toggle listening
//! Press Ctrl+C to exit

use regex::Regex;
use rodio::{Decoder, OutputStream, Sink};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, thread};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;

// ============================================================================
// Configuration
// ============================================================================

const SOCKET_PATH: &str = "/workspace/perception-voice/perception.sock";
const CLIENT_UID: &str = "whisper-notes";
const POLLING_INTERVAL_MS: u64 = 100;
const LISTENING_STATE_DELAY_MS: u64 = 200;

// Voice commands: phrase -> replacement
fn get_commands() -> Vec<(&'static str, &'static str)> {
    vec![("command enter", "\r\n")]
}

// Phrases to discard (common misheard sounds)
fn get_discard_phrases() -> HashSet<&'static str> {
    ["thank you", "thanks", "you"].into_iter().collect()
}

// ============================================================================
// IPC Client for perception-voice server
// ============================================================================

#[derive(Serialize)]
struct IpcRequest {
    command: String,
    uid: String,
}

#[derive(Deserialize)]
struct IpcResponse {
    status: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Transcription {
    #[serde(default)]
    text: String,
}

/// Send a message to the perception-voice server and receive response
/// Uses 4-byte big-endian length prefix framing
fn send_message(command: &str, uid: &str) -> Result<IpcResponse, String> {
    let mut stream =
        UnixStream::connect(SOCKET_PATH).map_err(|e| format!("Connect failed: {}", e))?;

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok();

    // Build message
    let request = IpcRequest {
        command: command.to_string(),
        uid: uid.to_string(),
    };
    let payload = serde_json::to_vec(&request).map_err(|e| format!("JSON encode failed: {}", e))?;

    // Send length prefix + payload
    let len = payload.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| format!("Write length failed: {}", e))?;
    stream
        .write_all(&payload)
        .map_err(|e| format!("Write payload failed: {}", e))?;

    // Read response length
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("Read length failed: {}", e))?;
    let response_len = u32::from_be_bytes(len_buf) as usize;

    // Read response payload
    let mut response_buf = vec![0u8; response_len];
    stream
        .read_exact(&mut response_buf)
        .map_err(|e| format!("Read payload failed: {}", e))?;

    // Parse response
    serde_json::from_slice(&response_buf).map_err(|e| format!("JSON decode failed: {}", e))
}

/// Check if perception-voice server is running
fn is_server_running() -> bool {
    std::path::Path::new(SOCKET_PATH).exists()
}

/// Set read marker to now (discard old transcriptions)
fn set_read_marker() -> bool {
    match send_message("set", CLIENT_UID) {
        Ok(response) => response.status == "ok",
        Err(e) => {
            eprintln!("Failed to set read marker: {}", e);
            false
        }
    }
}

/// Get transcriptions since last read marker
fn get_transcriptions() -> Vec<Transcription> {
    match send_message("get", CLIENT_UID) {
        Ok(response) => {
            if response.status != "ok" || response.text.is_empty() {
                return vec![];
            }
            // Parse JSONL
            response
                .text
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }
        Err(e) => {
            eprintln!("Failed to get transcriptions: {}", e);
            vec![]
        }
    }
}

// ============================================================================
// Sound Player
// ============================================================================

fn play_sound(path: &str) {
    let path = path.to_string();
    thread::spawn(move || {
        if let Ok(file) = File::open(&path) {
            if let Ok((_stream, stream_handle)) = OutputStream::try_default() {
                if let Ok(source) = Decoder::new(BufReader::new(file)) {
                    if let Ok(sink) = Sink::try_new(&stream_handle) {
                        sink.append(source);
                        sink.sleep_until_end();
                    }
                }
            }
        }
    });
}

// ============================================================================
// Text Processing
// ============================================================================

/// Check if text should be discarded
fn should_discard(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let normalized = text
        .to_lowercase()
        .trim()
        .trim_matches(|c: char| c.is_whitespace() || ".,!?;:".contains(c))
        .to_string();
    get_discard_phrases().contains(normalized.as_str())
}

/// Normalize text for command matching: lowercase and replace non-alpha with space
fn normalize_for_matching(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphabetic() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Process text and apply voice commands
fn process_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // Strip trailing period (Whisper adds them automatically)
    let cleaned = Regex::new(r"\.\s*$")
        .unwrap()
        .replace(text, "")
        .to_string();

    // Normalize for command matching
    let normalized = normalize_for_matching(&cleaned);

    // Check for voice commands (sorted by length, longest first)
    let mut commands = get_commands();
    commands.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    // Check if the entire text is a command
    for (phrase, replacement) in &commands {
        let normalized_phrase = normalize_for_matching(phrase);
        if normalized == normalized_phrase {
            // Entire utterance is the command - return just the replacement
            return replacement.to_string();
        }
    }

    // Check for commands within the text and replace them
    let mut result = cleaned;
    for (phrase, replacement) in &commands {
        // Build a pattern that matches the phrase with optional non-alpha separators
        let words: Vec<&str> = phrase.split_whitespace().collect();
        let pattern = words
            .iter()
            .map(|w| regex::escape(w))
            .collect::<Vec<_>>()
            .join(r"[^a-zA-Z]+");
        let pattern = format!(r"(?i)\b{}\b[,.\s]*", pattern);
        
        if let Ok(re) = Regex::new(&pattern) {
            result = re.replace_all(&result, *replacement).to_string();
        }
    }

    result
}

// ============================================================================
// X11 Global Hotkey
// ============================================================================

/// Start X11 global hotkey listener for Ctrl+Shift+Space
fn start_hotkey_listener(
    should_quit: Arc<AtomicBool>,
    toggle_tx: std::sync::mpsc::Sender<()>,
) -> Result<(), String> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("X11 connect failed: {}", e))?;

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // Key codes for modifiers and space
    // These are standard X11 keycodes
    const SPACE_KEYCODE: u8 = 65; // Space bar
    
    // Modifier masks
    let ctrl_mask = ModMask::CONTROL;
    let shift_mask = ModMask::SHIFT;
    let lock_mask = ModMask::LOCK;     // CapsLock
    let mod2_mask = ModMask::M2;       // NumLock (typically)

    // Grab key: Ctrl+Shift+Space on root window
    // We need to grab with different modifier combinations because of NumLock, CapsLock, etc.
    let base_mods = ctrl_mask | shift_mask;
    let modifiers: [ModMask; 4] = [
        base_mods,
        base_mods | lock_mask,
        base_mods | mod2_mask,
        base_mods | lock_mask | mod2_mask,
    ];

    for mods in modifiers {
        conn.grab_key(
            true,
            root,
            mods,
            SPACE_KEYCODE,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .map_err(|e| format!("Grab key failed: {}", e))?;
    }
    conn.flush().map_err(|e| format!("Flush failed: {}", e))?;

    println!("  (Global hotkey active via X11)");

    // Event loop
    thread::spawn(move || {
        while !should_quit.load(Ordering::Relaxed) {
            match conn.poll_for_event() {
                Ok(Some(event)) => {
                    if let Event::KeyPress(key_event) = event {
                        // Check if it's our hotkey
                        if key_event.detail == SPACE_KEYCODE {
                            let _ = toggle_tx.send(());
                        }
                    }
                }
                Ok(None) => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    break;
                }
            }
        }
    });

    Ok(())
}

// ============================================================================
// Main
// ============================================================================

/// Format duration as timestamp HH:MM:SS,mmm (SRT format)
fn format_timestamp(duration: Duration) -> String {
    let total_ms = duration.as_millis();
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let hours = total_mins / 60;
    format!("{:02}:{:02}:{:02},{:03}", hours, mins, secs, ms)
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 || args[1] == "--help" || args[1] == "-h" {
        println!(
            r#"
whisper-notes - Voice dictation CLI that appends to a file

Usage: whisper-notes [OPTIONS] <file>

Options:
  --ts <ms>    Enable timestamp mode for transcripts.
               Prefixes each utterance with elapsed time.
               <ms> is the gap (in milliseconds) before starting a new line.
               Example: --ts 2000 (2 second gap = new timestamped line)

Press Ctrl+Shift+Space to toggle listening
Say "command enter" to insert a new line
Press Ctrl+C to exit

Prerequisites:
  perception-voice serve must be running
"#
        );
        std::process::exit(if args.len() > 1 && (args[1] == "--help" || args[1] == "-h") { 0 } else { 1 });
    }

    // Parse arguments
    let mut output_file: Option<String> = None;
    let mut timestamp_gap_ms: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--ts" {
            if i + 1 < args.len() {
                timestamp_gap_ms = args[i + 1].parse().ok();
                if timestamp_gap_ms.is_none() {
                    eprintln!("Error: --ts requires a numeric value in milliseconds");
                    std::process::exit(1);
                }
                i += 2;
            } else {
                eprintln!("Error: --ts requires a value");
                std::process::exit(1);
            }
        } else if !args[i].starts_with('-') {
            output_file = Some(args[i].clone());
            i += 1;
        } else {
            eprintln!("Error: Unknown option {}", args[i]);
            std::process::exit(1);
        }
    }

    let output_file = match output_file {
        Some(f) => f,
        None => {
            eprintln!("Error: No output file specified");
            std::process::exit(1);
        }
    };
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let sound_on = exe_dir.join("sfx/squelch-on.wav");
    let sound_off = exe_dir.join("sfx/click-off.wav");

    println!("{}", "=".repeat(60));
    println!("whisper-notes - Voice Dictation to File");
    println!("{}", "=".repeat(60));
    println!("Output file: {}", output_file);
    if let Some(gap) = timestamp_gap_ms {
        println!("Timestamp mode: {}ms gap", gap);
    }

    // Check if perception-voice server is running
    if !is_server_running() {
        println!("⚠️  perception-voice server not found at {}", SOCKET_PATH);
        println!("   Make sure perception-voice serve is running");
    } else {
        println!("✓ perception-voice server detected");
    }

    // State
    let is_listening = Arc::new(AtomicBool::new(false));
    let should_quit = Arc::new(AtomicBool::new(false));

    // Channel for toggle events
    let (toggle_tx, toggle_rx) = std::sync::mpsc::channel::<()>();

    // Start hotkey listener
    println!("✓ Hotkey: ctrl+shift+space");
    if let Err(e) = start_hotkey_listener(
        Arc::clone(&should_quit),
        toggle_tx,
    ) {
        eprintln!("  (X11 hotkey failed: {})", e);
        eprintln!("  (Falling back to Ctrl+C only mode)");
    }

    // Setup Ctrl+C handler
    let quit_flag = Arc::clone(&should_quit);
    ctrlc::set_handler(move || {
        println!("\n⏹️  Stopping...");
        quit_flag.store(true, Ordering::Relaxed);
    })
    .expect("Error setting Ctrl+C handler");

    println!("🎙️  Ready! Press Ctrl+Shift+Space to toggle listening... (Ctrl+C to quit)");

    // Main loop
    let output_path = output_file.to_string();
    let sound_on_path = sound_on.to_string_lossy().to_string();
    let sound_off_path = sound_off.to_string_lossy().to_string();

    // Timestamp tracking
    let mut listening_start_time: Option<Instant> = None;
    let mut last_utterance_time: Option<Instant> = None;
    let mut is_first_utterance = true;

    while !should_quit.load(Ordering::Relaxed) {
        // Check for toggle events
        if toggle_rx.try_recv().is_ok() {
            let currently_listening = is_listening.load(Ordering::Relaxed);

            if currently_listening {
                // Stop listening
                is_listening.store(false, Ordering::Relaxed);
                println!("⏸️  Listening stopped");
                play_sound(&sound_off_path);
            } else {
                // Start listening
                if !is_server_running() {
                    println!("❌ perception-voice server not running!");
                } else {
                    is_listening.store(true, Ordering::Relaxed);
                    println!("🎤 Listening started");
                    play_sound(&sound_on_path);

                    // Set read marker to now (discard old transcriptions)
                    set_read_marker();

                    // Reset timestamp tracking
                    listening_start_time = Some(Instant::now());
                    last_utterance_time = None;
                    is_first_utterance = true;

                    // Delay before starting to poll
                    thread::sleep(Duration::from_millis(LISTENING_STATE_DELAY_MS));
                }
            }
        }

        // Poll for transcriptions if listening
        if is_listening.load(Ordering::Relaxed) {
            let transcriptions = get_transcriptions();

            for item in transcriptions {
                if !is_listening.load(Ordering::Relaxed) {
                    println!("[Cancelled]: {}", item.text);
                    break;
                }

                let text = item.text.trim();
                if text.is_empty() {
                    continue;
                }

                if should_discard(text) {
                    println!("[Discarded]: {}", text);
                    continue;
                }

                // Process and append to file
                let processed = process_text(text);
                if processed.is_empty() {
                    continue;
                }

                // Build the text to write
                let to_write = if let Some(gap_ms) = timestamp_gap_ms {
                    // Timestamp mode
                    let now = Instant::now();
                    let elapsed = listening_start_time
                        .map(|start| now.duration_since(start))
                        .unwrap_or(Duration::ZERO);
                    
                    let needs_new_line = if is_first_utterance {
                        is_first_utterance = false;
                        true
                    } else if let Some(last_time) = last_utterance_time {
                        now.duration_since(last_time).as_millis() > gap_ms as u128
                    } else {
                        true
                    };
                    
                    last_utterance_time = Some(now);
                    
                    if needs_new_line {
                        // New timestamped line
                        format!("\n[{}] {}", format_timestamp(elapsed), processed)
                    } else {
                        // Continue on same line
                        format!(" {}", processed)
                    }
                } else {
                    // Normal mode - append with trailing space
                    if processed.trim().is_empty() {
                        processed.clone()
                    } else {
                        format!("{} ", processed)
                    }
                };

                match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&output_path)
                {
                    Ok(mut file) => {
                        if let Err(e) = file.write_all(to_write.as_bytes()) {
                            eprintln!("Failed to write to file: {}", e);
                        } else {
                            let display = to_write.replace("\r\n", "⏎\n").replace('\n', "⏎\n");
                            println!("[Appended]: {} → {}", text, display.trim());
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to open file: {}", e);
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(POLLING_INTERVAL_MS));
    }
}
