//! Native clipboard probing for the interactive prompt.
//!
//! The Rust port intentionally uses documented OS clipboard programs rather
//! than pretending a clipboard exists. Every probe returns an explicit
//! failure/`None` when the backend is absent, and image bytes are validated
//! before they are written into a prompt attachment.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use uuid::Uuid;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CLIPBOARD_BYTES: usize = 50 * 1024 * 1024;
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardError(pub String);

pub fn is_wayland_session(env: &[(String, String)]) -> bool {
    env.iter().any(|(key, value)| {
        (key == "WAYLAND_DISPLAY" && !value.is_empty())
            || (key == "XDG_SESSION_TYPE" && value == "wayland")
    })
}

fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn current_env() -> Vec<(String, String)> {
    std::env::vars().collect()
}

fn run_command_with_env(
    command: &str,
    args: &[&str],
    input: Option<&[u8]>,
    env: &[(String, String)],
) -> Result<Vec<u8>, ClipboardError> {
    // Tests and embedding callers need an explicit environment without
    // changing process-global state. The production path passes the inherited
    // environment explicitly as well, keeping probe behavior deterministic.
    let mut child = Command::new(command);
    child.args(args).env_clear().envs(env.iter().cloned());
    let mut child = child
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ClipboardError(format!("{command}: {error}")))?;
    let stdout = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout
                .take((MAX_CLIPBOARD_BYTES + 1) as u64)
                .read_to_end(&mut bytes);
            bytes
        })
    });
    // Do not write synchronously before the timeout loop: a clipboard helper
    // that exits early or never reads stdin must not hang the interactive
    // prompt or leak a child process. A closed pipe is also not itself a
    // failure when the helper exited successfully (wl-copy can do that).
    let writer = input.and_then(|input| {
        child.stdin.take().map(|mut stdin| {
            let input = input.to_vec();
            std::thread::spawn(move || stdin.write_all(&input))
        })
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(writer) = writer {
                    let _ = writer.join();
                }
                return Err(ClipboardError(format!("{command} timed out")));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(writer) = writer {
                    let _ = writer.join();
                }
                return Err(ClipboardError(format!("wait for {command}: {error}")));
            }
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let bytes = stdout
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default();
    if !status.success() {
        return Err(ClipboardError(format!("{command} exited with {status}")));
    }
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return Err(ClipboardError(format!(
            "{command} output exceeds clipboard limit"
        )));
    }
    Ok(bytes)
}

fn command_env() -> Vec<(String, String)> {
    current_env()
}

fn has_nonempty_env(env: &[(String, String)], key: &str) -> bool {
    env_value(env, key).is_some_and(|value| !value.is_empty())
}

fn read_text_sync(env: &[(String, String)], platform: &str) -> Option<String> {
    if platform == "linux" && is_wayland_session(env) && has_nonempty_env(env, "WAYLAND_DISPLAY") {
        if let Ok(bytes) =
            run_command_with_env("wl-paste", &["--no-newline", "--type", "text"], None, env)
        {
            // An empty Wayland clipboard is authoritative; do not substitute
            // stale X11 text, matching the upstream regression fix.
            return (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned());
        }
    }

    let result = match platform {
        "darwin" | "macos" => run_command_with_env("pbpaste", &[], None, env),
        "windows" => run_command_with_env(
            "powershell.exe",
            &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
            None,
            env,
        ),
        "linux" => {
            if has_nonempty_env(env, "DISPLAY") {
                run_command_with_env("xclip", &["-selection", "clipboard", "-o"], None, env)
                    .or_else(|_| {
                        run_command_with_env("xsel", &["--clipboard", "--output"], None, env)
                    })
            } else {
                Err(ClipboardError("no clipboard display".into()))
            }
        }
        _ => Err(ClipboardError("unsupported platform clipboard".into())),
    };
    result
        .ok()
        .and_then(|bytes| (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned()))
}

pub async fn read_clipboard_text() -> Option<String> {
    tokio::task::spawn_blocking(|| read_text_sync(&command_env(), std::env::consts::OS))
        .await
        .ok()
        .flatten()
}

/// Environment-injected synchronous probe used by process-level tests and
/// embedders. It exercises the same backend order as the production wrapper.
pub fn read_clipboard_text_with_env(env: &[(String, String)], platform: &str) -> Option<String> {
    read_text_sync(env, platform)
}

fn is_remote_session(env: &[(String, String)]) -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"]
        .iter()
        .any(|key| has_nonempty_env(env, key))
}

fn is_wsl_session(env: &[(String, String)]) -> bool {
    // Keep this environment-injected, just like the upstream helper, so the
    // image path can be tested without changing the process environment.
    has_nonempty_env(env, "WSL_DISTRO_NAME") || has_nonempty_env(env, "WSLENV")
}

fn emit_osc52(text: &str, env: &[(String, String)]) -> bool {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH || has_nonempty_env(env, "PI_DISABLE_OSC52") {
        return false;
    }
    // A terminal advertising TERM is a real clipboard transport. Callers can
    // disable this explicitly for deterministic headless tests.
    if !has_nonempty_env(env, "TERM") {
        return false;
    }
    print!("\x1b]52;c;{encoded}\x07");
    let _ = std::io::stdout().flush();
    true
}

fn copy_sync(text: String, env: &[(String, String)], platform: &str) -> Result<(), ClipboardError> {
    let mut copied = false;
    let bytes = text.as_bytes();
    if platform == "darwin" || platform == "macos" {
        copied = run_command_with_env("pbcopy", &[], Some(bytes), env).is_ok();
    } else if platform == "windows" {
        copied = run_command_with_env("clip", &[], Some(bytes), env).is_ok();
    } else if platform == "linux" {
        if has_nonempty_env(env, "TERMUX_VERSION") {
            copied = run_command_with_env("termux-clipboard-set", &[], Some(bytes), env).is_ok();
        }
        if !copied && is_wayland_session(env) && has_nonempty_env(env, "WAYLAND_DISPLAY") {
            copied = run_command_with_env("wl-copy", &[], Some(bytes), env).is_ok();
        }
        if !copied && has_nonempty_env(env, "DISPLAY") {
            copied = run_command_with_env("xclip", &["-selection", "clipboard"], Some(bytes), env)
                .is_ok();
            if !copied {
                copied =
                    run_command_with_env("xsel", &["--clipboard", "--input"], Some(bytes), env)
                        .is_ok();
            }
        }
    }

    let remote = is_remote_session(env);
    if (remote || !copied) && emit_osc52(&text, env) {
        copied = true;
    }
    if copied {
        Ok(())
    } else {
        Err(ClipboardError("no working clipboard backend".into()))
    }
}

pub async fn copy_to_clipboard(text: &str) -> Result<(), ClipboardError> {
    let text = text.to_string();
    tokio::task::spawn_blocking(move || copy_sync(text, &command_env(), std::env::consts::OS))
        .await
        .map_err(|error| ClipboardError(format!("clipboard task failed: {error}")))?
}

/// Synchronous, environment-injected entry point for process-level tests and
/// embedders. The production wrapper above uses the real process environment.
pub fn copy_to_clipboard_sync_with_env(
    text: &str,
    env: &[(String, String)],
    platform: &str,
) -> Result<(), ClipboardError> {
    copy_sync(text.to_string(), env, platform)
}

fn base_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
}

pub fn extension_for_image_mime_type(mime: &str) -> Option<&'static str> {
    match base_mime(mime).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

fn preferred_image_type(types: impl Iterator<Item = String>) -> Option<String> {
    let types: Vec<(String, String)> = types
        .map(|raw| {
            let raw = raw.trim().to_string();
            let base = base_mime(&raw);
            (raw, base)
        })
        .filter(|(raw, base)| !raw.is_empty() && !base.is_empty())
        .collect();
    for preferred in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        if let Some((raw, _)) = types.iter().find(|(_, base)| base == preferred) {
            return Some(raw.clone());
        }
    }
    types
        .into_iter()
        .find(|(_, base)| base.starts_with("image/"))
        .map(|(raw, _)| raw)
}

fn valid_image_bytes(bytes: &[u8], mime: &str) -> bool {
    match base_mime(mime).as_str() {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        "image/bmp" => bytes.starts_with(b"BM"),
        _ => false,
    }
}

fn read_wayland_image(env: &[(String, String)]) -> Option<ClipboardImage> {
    let types = run_command_with_env("wl-paste", &["--list-types"], None, env).ok()?;
    let selected =
        preferred_image_type(String::from_utf8_lossy(&types).lines().map(str::to_string))?;
    let selected_base = base_mime(&selected);
    let bytes = run_command_with_env(
        "wl-paste",
        &["--type", &selected, "--no-newline"],
        None,
        env,
    )
    .ok()?;
    (!bytes.is_empty()).then_some(ClipboardImage {
        bytes,
        mime_type: selected_base,
    })
}

fn read_xclip_image(env: &[(String, String)]) -> Option<ClipboardImage> {
    let targets = run_command_with_env(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        None,
        env,
    )
    .ok();
    let preferred = targets.as_deref().and_then(|bytes| {
        preferred_image_type(String::from_utf8_lossy(bytes).lines().map(str::to_string))
    });
    let candidates = preferred.into_iter().chain(
        [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "image/bmp",
        ]
        .iter()
        .map(|value| value.to_string()),
    );
    for mime in candidates {
        let mime_ref = mime.as_str();
        if let Ok(bytes) = run_command_with_env(
            "xclip",
            &["-selection", "clipboard", "-t", mime_ref, "-o"],
            None,
            env,
        ) {
            if !bytes.is_empty() {
                return Some(ClipboardImage {
                    bytes,
                    mime_type: base_mime(mime_ref),
                });
            }
        }
    }
    None
}

/// Read a Windows screenshot from the clipboard when running under WSL.
/// Linux clipboard tools do not consistently expose Win+Shift+S data, while
/// PowerShell can save the Windows clipboard image directly to a WSL path.
/// The temporary file is always removed before returning to the caller.
fn read_wsl_image(env: &[(String, String)]) -> Option<ClipboardImage> {
    let tmp_path = std::env::temp_dir().join(format!("pi-wsl-clip-{}.png", Uuid::new_v4()));
    let result = (|| {
        let tmp_path_string = tmp_path.to_string_lossy().into_owned();
        let win_path =
            run_command_with_env("wslpath", &["-w", tmp_path_string.as_str()], None, env).ok()?;
        let win_path = String::from_utf8_lossy(&win_path).trim().to_string();
        if win_path.is_empty() {
            return None;
        }
        let powershell_path = win_path.replace('\'', "''");
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $path = '{powershell_path}'; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img) {{ $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' }} else {{ Write-Output 'empty' }}"
        );
        let output = run_command_with_env(
            "powershell.exe",
            &["-NoProfile", "-Command", script.as_str()],
            None,
            env,
        )
        .ok()?;
        if String::from_utf8_lossy(&output).trim() != "ok" {
            return None;
        }
        let bytes = fs::read(&tmp_path).ok()?;
        (!bytes.is_empty()).then_some(ClipboardImage {
            bytes,
            mime_type: "image/png".to_string(),
        })
    })();
    let _ = fs::remove_file(&tmp_path);
    result
}

/// Encode a small uncompressed 24/32-bit BMP as a standards-compliant PNG.
/// This covers the WSLg screenshot format without pulling a native image
/// runtime into the Rust-only binary.
pub fn convert_bmp_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 54 || !bytes.starts_with(b"BM") {
        return None;
    }
    let read_u16 = |offset: usize| {
        bytes
            .get(offset..offset + 2)
            .map(|v| u16::from_le_bytes([v[0], v[1]]))
    };
    let read_u32 = |offset: usize| {
        bytes
            .get(offset..offset + 4)
            .map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
    };
    let read_i32 = |offset: usize| {
        bytes
            .get(offset..offset + 4)
            .map(|v| i32::from_le_bytes([v[0], v[1], v[2], v[3]]))
    };
    let data_offset = read_u32(10)? as usize;
    let dib_size = read_u32(14)?;
    let width = read_i32(18)?;
    let height = read_i32(22)?;
    let planes = read_u16(26)?;
    let bpp = read_u16(28)?;
    let compression = read_u32(30)?;
    if dib_size < 40
        || width <= 0
        || height == 0
        || planes != 1
        || compression != 0
        || !matches!(bpp, 24 | 32)
    {
        return None;
    }
    let width = width as usize;
    let bottom_up = height > 0;
    let height = height.unsigned_abs() as usize;
    let bytes_per_pixel = (bpp / 8) as usize;
    let row_stride = (width * bytes_per_pixel + 3) & !3;
    let total = data_offset.checked_add(row_stride.checked_mul(height)?)?;
    if total > bytes.len() || width > 16_384 || height > 16_384 {
        return None;
    }
    let mut raw = Vec::with_capacity((width * 4 + 1) * height);
    for output_row in 0..height {
        let source_row = if bottom_up {
            height - 1 - output_row
        } else {
            output_row
        };
        let row = &bytes[data_offset + source_row * row_stride..];
        raw.push(0);
        for pixel in 0..width {
            let index = pixel * bytes_per_pixel;
            let b = row[index];
            let g = row[index + 1];
            let r = row[index + 2];
            let a = if bpp == 32 { row[index + 3] } else { 255 };
            raw.extend_from_slice(&[r, g, b, a]);
        }
    }
    png_from_rgba(width as u32, height as u32, &raw)
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(kind);
    crc_data.extend_from_slice(data);
    output.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

fn png_from_rgba(width: u32, height: u32, raw: &[u8]) -> Option<Vec<u8>> {
    let mut zlib = vec![0x78, 0x01]; // deflate, no compression, fastest safe path
    let mut remaining = raw;
    while !remaining.is_empty() {
        let length = remaining.len().min(65_535);
        let final_block = length == remaining.len();
        zlib.push(u8::from(final_block));
        zlib.extend_from_slice(&(length as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(length as u16)).to_le_bytes());
        zlib.extend_from_slice(&remaining[..length]);
        remaining = &remaining[length..];
    }
    if raw.is_empty() {
        zlib.push(1);
        zlib.extend_from_slice(&0u16.to_le_bytes());
        zlib.extend_from_slice(&u16::MAX.to_le_bytes());
    }
    zlib.extend_from_slice(&adler32(raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(b"IHDR", &ihdr, &mut png);
    png_chunk(b"IDAT", &zlib, &mut png);
    png_chunk(b"IEND", &[], &mut png);
    Some(png)
}

pub async fn read_clipboard_image() -> Option<ClipboardImage> {
    let env = command_env();
    tokio::task::spawn_blocking(move || read_clipboard_image_sync(&env, std::env::consts::OS))
        .await
        .ok()
        .flatten()
}

pub fn read_clipboard_image_sync(
    env: &[(String, String)],
    platform: &str,
) -> Option<ClipboardImage> {
    if has_nonempty_env(env, "TERMUX_VERSION") {
        return None;
    }
    let wsl = platform == "linux" && is_wsl_session(env);
    let mut image = if platform == "linux" {
        if is_wayland_session(env) || wsl {
            read_wayland_image(env).or_else(|| read_xclip_image(env))
        } else if has_nonempty_env(env, "DISPLAY") {
            read_xclip_image(env)
        } else {
            None
        }
    } else {
        // There is no linked native clipboard crate in the Rust-only binary;
        // returning None is safer than claiming a platform image backend.
        None
    };

    if image.is_none() && wsl {
        image = read_wsl_image(env);
    }
    let mut image = image?;

    if !valid_image_bytes(&image.bytes, &image.mime_type) {
        if base_mime(&image.mime_type) == "image/bmp" {
            let png = convert_bmp_to_png(&image.bytes)?;
            image.bytes = png;
            image.mime_type = "image/png".to_string();
        } else {
            return None;
        }
    }
    Some(image)
}

pub fn write_image_attachment(image: &ClipboardImage) -> Result<PathBuf, ClipboardError> {
    let extension = extension_for_image_mime_type(&image.mime_type).unwrap_or("png");
    let path = std::env::temp_dir().join(format!("pi-clipboard-{}.{}", Uuid::new_v4(), extension));
    fs::write(&path, &image.bytes)
        .map_err(|error| ClipboardError(format!("write clipboard image: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ClipboardError(format!("protect clipboard image: {error}")))?;
    }
    Ok(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn env(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn bmp_1x1() -> Vec<u8> {
        let mut bytes = vec![0u8; 58];
        bytes[0..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&(58u32).to_le_bytes());
        bytes[10..14].copy_from_slice(&(54u32).to_le_bytes());
        bytes[14..18].copy_from_slice(&(40u32).to_le_bytes());
        bytes[18..22].copy_from_slice(&(1i32).to_le_bytes());
        bytes[22..26].copy_from_slice(&(1i32).to_le_bytes());
        bytes[26..28].copy_from_slice(&(1u16).to_le_bytes());
        bytes[28..30].copy_from_slice(&(24u16).to_le_bytes());
        bytes[34..38].copy_from_slice(&(4u32).to_le_bytes());
        bytes[56] = 255;
        bytes
    }

    #[test]
    fn converts_bmp_to_png_with_valid_signature() {
        let png = convert_bmp_to_png(&bmp_1x1()).expect("valid bmp");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn malformed_images_are_rejected() {
        assert!(convert_bmp_to_png(b"BM").is_none());
        assert!(!valid_image_bytes(b"not png", "image/png"));
    }

    #[test]
    fn wayland_detection_requires_wayland_signal() {
        assert!(is_wayland_session(&env(&[("XDG_SESSION_TYPE", "wayland")])));
        assert!(!is_wayland_session(&env(&[("DISPLAY", ":0")])));
    }

    #[test]
    fn wsl_detection_uses_nonempty_environment_markers() {
        assert!(is_wsl_session(&env(&[("WSL_DISTRO_NAME", "Ubuntu")])));
        assert!(is_wsl_session(&env(&[("WSLENV", "PATH/l")])));
        assert!(!is_wsl_session(&env(&[("WSL_DISTRO_NAME", "")])));
    }

    #[cfg(unix)]
    #[test]
    fn wsl_image_falls_back_to_powershell_and_removes_temp_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("pi-wsl-clipboard-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).expect("create fixture directory");
        let write_script = |name: &str, body: &str| {
            let path = root.join(name);
            fs::write(&path, body).expect("write fixture command");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("make fixture command executable");
        };
        write_script("wl-paste", "#!/bin/sh\nexit 1\n");
        write_script("xclip", "#!/bin/sh\nexit 1\n");
        write_script("wslpath", "#!/bin/sh\nprintf '%s' \"$2\"\n");
        write_script(
            "powershell.exe",
            r##"#!/bin/sh
path=$(printf '%s' "$3" | sed -n 's/.*[$]path = //p' | cut -d "'" -f2)
printf '\211PNG\r\n\032\n' > "$path"
printf 'ok\n'
"##,
        );
        let environment = vec![
            (
                "PATH".to_string(),
                format!("{}:/usr/bin:/bin", root.display()),
            ),
            ("WSL_DISTRO_NAME".to_string(), "Ubuntu".to_string()),
        ];

        let image = read_clipboard_image_sync(&environment, "linux").expect("WSL image");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.bytes, b"\x89PNG\r\n\x1a\n");
        let _ = fs::remove_dir_all(root);
    }
}
