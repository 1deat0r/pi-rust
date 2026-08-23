//! Read tool — port of `packages/agent/src/harness/tools/read.ts` (text
//! path with offset/limit + truncation; image path returns base64 blocks).

use super::image::{process_image, ProcessImageOptions};
use super::path_utils::resolve_read_tool_path_existing;
use super::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};
use pi_ai::types::ContentBlock;
use pi_ai::types::ToolResultMessage;

pub use super::image::detect_supported_image_mime_type;

/// Tool execute handler for `read`: text truncation messages ported from
/// upstream; images pass through as base64 content blocks.
pub async fn execute_read(
    tool_call_id: &str,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    execute_read_with_options(
        tool_call_id,
        path,
        offset,
        limit,
        cwd,
        ProcessImageOptions::default(),
    )
    .await
}

pub async fn execute_read_with_options(
    tool_call_id: &str,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    cwd: &str,
    image_options: ProcessImageOptions,
) -> Result<ToolResultMessage, String> {
    let absolute = resolve_read_tool_path_existing(cwd, path);
    let bytes = std::fs::read(&absolute).map_err(|e| format!("Failed to read {path}: {e}"))?;

    if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
        match process_image(&bytes, mime_type, image_options) {
            Ok(processed) => {
                let mut text = format!("Read image file [{}]", processed.mime_type);
                if !processed.hints.is_empty() {
                    text.push('\n');
                    text.push_str(&processed.hints.join("\n"));
                }
                return Ok(ToolResultMessage::new(
                    tool_call_id,
                    "read",
                    vec![
                        ContentBlock::text(text),
                        ContentBlock::image(processed.data, processed.mime_type),
                    ],
                    false,
                ));
            }
            Err(message) => {
                return Ok(ToolResultMessage::text(
                    tool_call_id,
                    "read",
                    format!("Read image file [{mime_type}]\n{message}"),
                    false,
                ));
            }
        }
    }

    let text_content = String::from_utf8_lossy(&bytes).into_owned();
    let all_lines: Vec<&str> = text_content.split('\n').collect();
    let total_file_lines = all_lines.len();
    let start_line = match offset {
        Some(o) => (o as usize).saturating_sub(1),
        None => 0,
    };
    let start_line_display = start_line + 1;
    if start_line >= all_lines.len() {
        return Err(format!(
            "Offset {} is beyond end of file ({} lines total)",
            offset.unwrap_or(0.0) as usize,
            all_lines.len()
        ));
    }

    let (selected_content, user_limited_lines): (String, Option<usize>) = if let Some(limit) = limit
    {
        let end_line = (start_line + limit as usize).min(all_lines.len());
        (
            all_lines[start_line..end_line].join("\n"),
            Some(end_line - start_line),
        )
    } else {
        (all_lines[start_line..].join("\n"), None)
    };

    let truncation = truncate_head(&selected_content);
    let mut output_text: String;
    if truncation.truncated_by == Some(super::truncate::TruncatedBy::Bytes)
        && truncation.output_lines == 0
    {
        let first_line_size = format_size(utf8_len(all_lines[start_line]));
        output_text = format!(
            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {}]",
            format_size(DEFAULT_MAX_BYTES),
            DEFAULT_MAX_BYTES
        );
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        output_text = truncation.content.clone();
        if truncation.truncated_by == Some(super::truncate::TruncatedBy::Lines) {
            output_text += &format!(
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
            );
        } else {
            output_text += &format!(
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            );
        }
    } else if let Some(user_limited_lines) = user_limited_lines {
        if start_line + user_limited_lines < all_lines.len() {
            let remaining = all_lines.len() - (start_line + user_limited_lines);
            let next_offset = start_line + user_limited_lines + 1;
            output_text = format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            );
        } else {
            output_text = truncation.content.clone();
        }
    } else {
        output_text = truncation.content.clone();
    }

    Ok(ToolResultMessage::text(
        tool_call_id,
        "read",
        output_text,
        false,
    ))
}

fn utf8_len(s: &str) -> usize {
    s.len()
}
