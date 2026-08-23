//! List available models with optional fuzzy search — port of
//! `packages/coding-agent/src/cli/list-models.ts`.
//!
//! Uses the pi-ai Models facade over the built-in provider registry. Model
//! availability is auth-gated: providers without an env key or stored
//! credential are excluded (upstream `getAvailable`). The table format
//! mirrors upstream exactly (provider / model / context / max-out / thinking
//! / images columns).

use pi_ai::models::Models;

/// Format a number as human-readable (e.g., 200000 -> "200K", 1000000 -> "1M").
fn format_token_count(count: u64) -> String {
    if count >= 1_000_000 {
        let millions = count as f64 / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = count as f64 / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{thousands:.0}K")
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

/// Simple substring filter standing in for the TUI fuzzy filter
/// (upstream `fuzzyFilter(models, pattern, (m) => provider + " " + id)`).
fn filter_models<'a>(
    models: &'a [pi_ai::model::Model],
    pattern: &str,
) -> Vec<&'a pi_ai::model::Model> {
    let pattern = pattern.to_lowercase();
    models
        .iter()
        .filter(|m| {
            format!("{} {}", m.provider, m.id)
                .to_lowercase()
                .contains(&pattern)
        })
        .collect()
}

/// List available models, optionally filtered by search pattern.
pub fn list_models(models: &Models, search_pattern: Option<&str>) -> String {
    let all = models.get_available(None);
    if all.is_empty() {
        return crate::core::auth_guidance::format_no_models_available_message();
    }

    let filtered = match search_pattern {
        Some(p) if !p.is_empty() => filter_models(&all, p),
        _ => all.iter().collect(),
    };
    if filtered.is_empty() {
        return format!("No models matching \"{}\"", search_pattern.unwrap_or_default());
    }

    // Sort by provider, then by model id (upstream localeCompare).
    let mut rows: Vec<&pi_ai::model::Model> = filtered;
    rows.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.id.cmp(&b.id))
    });

    let headers = ["provider", "model", "context", "max-out", "thinking", "images"];
    let row_render: Vec<[String; 6]> = rows
        .iter()
        .map(|m| {
            [
                m.provider.clone(),
                m.id.clone(),
                format_token_count(m.context_window),
                format_token_count(m.max_tokens),
                if m.reasoning { "yes".to_string() } else { "no".to_string() },
                if m.input.contains(&pi_ai::model::ModelInput::Image) {
                    "yes".to_string()
                } else {
                    "no".to_string()
                },
            ]
        })
        .collect();

    let mut widths = [0usize; 6];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &row_render {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect::<Vec<_>>()
        .join("  ");
    out.push_str(&header_line);
    out.push('\n');
    for row in &row_render {
        let line = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:<width$}", cell, width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::model::{Model, ModelInput};

    fn model(provider: &str, id: &str, ctx: u64, max_tokens: u64, reasoning: bool, images: bool) -> Model {
        let mut m = Model::new(id, id, "test-api", provider);
        m.context_window = ctx;
        m.max_tokens = max_tokens;
        m.reasoning = reasoning;
        m.input = if images { vec![ModelInput::Text, ModelInput::Image] } else { vec![ModelInput::Text] };
        m
    }

    #[test]
    fn format_token_counts() {
        assert_eq!(format_token_count(200_000), "200K");
        assert_eq!(format_token_count(1_000_000), "1M");
        assert_eq!(format_token_count(1_500_000), "1.5M");
        assert_eq!(format_token_count(8192), "8.2K");
        assert_eq!(format_token_count(900), "900");
    }

    #[test]
    fn filtering_and_sorting() {
        let ms = vec![
            model("openai", "gpt-4o", 128_000, 16_384, false, true),
            model("anthropic", "claude-sonnet-4-6", 1_000_000, 128_000, true, true),
            model("google", "gemini-2.5-flash", 1_000_000, 65_536, true, true),
        ];
        let f = filter_models(&ms, "gemini");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].id, "gemini-2.5-flash");
    }

    #[test]
    fn empty_models_message() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
            let out = list_models(&models, None);
            assert!(out.contains("No models available"));
        });
    }
}
