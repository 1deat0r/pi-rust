#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real child-process acceptance coverage for the interactive editor,
//! clipboard/image, and Mermaid fallback boundaries.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use pi_coding_agent::interactive::clipboard::{
    convert_bmp_to_png, copy_to_clipboard_sync_with_env, read_clipboard_image_sync,
    read_clipboard_text_with_env,
};
use pi_coding_agent::interactive::external_editor::{
    edit_in_external_editor_blocking, ExternalEditorOptions, ExternalEditorResult,
};
use pi_coding_agent::interactive::mermaid::transform_markdown;

struct Sandbox {
    root: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pi-editor-clipboard-mermaid-{}",
            uuid::Uuid::new_v4()
        ));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        Self { root, bin }
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn env(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.retain(|(key, _)| key != "DISPLAY" && key != "WAYLAND_DISPLAY");
        env.push((
            "PATH".into(),
            format!("{}:/usr/bin:/bin", self.bin.display()),
        ));
        env.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into())),
        );
        env
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn png_1x1() -> &'static [u8] {
    &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

#[test]
fn external_editor_success_failure_cancellation_and_cleanup_are_real() {
    let sandbox = Sandbox::new();
    let capture = sandbox.root.join("editor-path");
    let success = sandbox.script(
        "editor-success",
        &format!(
            "printf '\\357\\273\\277edited\\n' > \"$1\"; printf '%s' \"$1\" > {}",
            capture.display()
        ),
    );
    let result = edit_in_external_editor_blocking(ExternalEditorOptions {
        command: success.display().to_string(),
        content: "original".into(),
    });
    assert_eq!(result, ExternalEditorResult::Complete("edited".into()));
    let prompt_path = fs::read_to_string(&capture).unwrap();
    assert!(!Path::new(&prompt_path).exists());
    assert!(prompt_path.ends_with("/prompt.md"));

    let failure = sandbox.script("editor-failure", "exit 17");
    assert!(matches!(
        edit_in_external_editor_blocking(ExternalEditorOptions {
            command: failure.display().to_string(),
            content: "original".into(),
        }),
        ExternalEditorResult::Failed(_)
    ));

    let cancelled_path = sandbox.root.join("cancelled-editor-path");
    let cancelled = sandbox.script(
        "editor-cancelled",
        &format!(
            "printf '%s' \"$1\" > \"{}\"; kill -INT $$",
            cancelled_path.display()
        ),
    );
    assert_eq!(
        edit_in_external_editor_blocking(ExternalEditorOptions {
            command: cancelled.display().to_string(),
            content: "original".into(),
        }),
        ExternalEditorResult::Cancelled
    );
    let cancelled_prompt_path = fs::read_to_string(cancelled_path).unwrap();
    assert!(!Path::new(&cancelled_prompt_path).exists());
}

#[test]
fn clipboard_process_probes_text_image_and_missing_backends() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.root.join("image.png"), png_1x1()).unwrap();
    sandbox.script(
        "wl-paste",
        &format!(
            "if [ \"$1\" = \"--list-types\" ]; then printf 'text/plain\\nimage/png\\n'; \
             elif [ \"$3\" = \"text\" ]; then printf 'from-wayland'; \
             else cat {}; fi",
            sandbox.root.join("image.png").display()
        ),
    );
    let wayland = sandbox.env(&[("WAYLAND_DISPLAY", "wayland-0")]);
    assert_eq!(
        read_clipboard_text_with_env(&wayland, "linux"),
        Some("from-wayland".into())
    );
    let image = read_clipboard_image_sync(&wayland, "linux").expect("valid clipboard image");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.bytes, png_1x1());

    sandbox.script(
        "wl-paste",
        "if [ \"$1\" = \"--list-types\" ]; then printf 'image/png\\n'; else printf 'malformed'; fi",
    );
    assert!(read_clipboard_image_sync(&wayland, "linux").is_none());

    let missing = sandbox.env(&[
        ("WAYLAND_DISPLAY", "wayland-0"),
        ("PI_DISABLE_OSC52", "1"),
        ("TERM", ""),
    ]);
    assert!(copy_to_clipboard_sync_with_env("text", &missing, "linux").is_err());
}

#[test]
fn bmp_conversion_and_mermaid_truthful_fallback_are_real() {
    let mut bmp = vec![0u8; 58];
    bmp[0..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&(58u32).to_le_bytes());
    bmp[10..14].copy_from_slice(&(54u32).to_le_bytes());
    bmp[14..18].copy_from_slice(&(40u32).to_le_bytes());
    bmp[18..22].copy_from_slice(&(1i32).to_le_bytes());
    bmp[22..26].copy_from_slice(&(1i32).to_le_bytes());
    bmp[26..28].copy_from_slice(&(1u16).to_le_bytes());
    bmp[28..30].copy_from_slice(&(24u16).to_le_bytes());
    bmp[34..38].copy_from_slice(&(4u32).to_le_bytes());
    bmp[56] = 0xff;
    let png = convert_bmp_to_png(&bmp).expect("valid bmp");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(convert_bmp_to_png(b"bad").is_none());

    let rendered = transform_markdown(
        "```mermaid\nflowchart LR\nA[Start] --> B[Done]\n```",
        100,
        "streaming",
    );
    assert!(rendered.contains("┌───────┐"));
    let unsupported = transform_markdown("```mermaid\npie\n```", 100, "streaming");
    assert_eq!(unsupported, "```mermaid\npie\n```");
    assert!(unsupported.contains("```mermaid"));
}
