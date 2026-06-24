//! `ai-memory recall-stats` — measure how often agents READ memory back.
//!
//! Thin HTTP client. Calls `GET /admin/recall-stats` on the configured
//! server and renders the response as human text or JSON. Never opens the
//! store directly — the server is the source of truth.
//!
//! This is the retrieval half of ai-memory's capture/recall asymmetry:
//! lifecycle hooks capture every prompt + tool call automatically, but
//! consulting memory is a voluntary agent action. This command answers the
//! question the system otherwise can't: are agents actually reading memory
//! back, or only writing to it?

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::RecallStatsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Server-shaped response. Mirrors `ai_memory_mcp::admin::RecallStatsReport`.
#[derive(Debug, Deserialize, Serialize)]
struct Report {
    window_days: u32,
    window: RecallStats,
    lifetime: RecallStats,
}

/// One recall slice. Mirrors `ai_memory_store::RecallStats`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct RecallStats {
    days: u32,
    sessions: u64,
    user_prompts: u64,
    tool_calls: u64,
    recall_calls: u64,
    memory_other_calls: u64,
    #[serde(default)]
    by_tool: Vec<RecallToolCount>,
    #[serde(default)]
    recall_per_prompt: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RecallToolCount {
    tool: String,
    count: u64,
}

/// Run the `recall-stats` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable, returns non-2xx, or the
/// response can't be parsed.
pub async fn run(config: &Config, args: RecallStatsArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config(config);
    let days = args.days.unwrap_or(30);
    let days_s = days.to_string();
    let report: Report = get_json(&ep, "/admin/recall-stats", &[("days", days_s.as_str())]).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("ai-memory recall-stats ({})", ep.url);
        print_slice(&format!("last {} days", report.window_days), &report.window);
        println!();
        print_slice("lifetime", &report.lifetime);
    }
    Ok(())
}

/// Render one recall slice as a small human-readable block.
fn print_slice(label: &str, s: &RecallStats) {
    println!("  [{label}]");
    println!("    sessions:        {}", s.sessions);
    println!("    user prompts:    {}", s.user_prompts);
    println!("    tool calls:      {}", s.tool_calls);
    println!(
        "    memory RECALL:   {}  ({} calls/prompt)",
        s.recall_calls,
        format_per_prompt(s.recall_per_prompt)
    );
    println!("    other memory:    {}", s.memory_other_calls);
    if s.by_tool.is_empty() {
        println!("      (no recall tools were called)");
    } else {
        for t in &s.by_tool {
            println!("      - {:<18} {}", t.tool, t.count);
        }
    }
}

/// Format the recall-per-prompt density. It is a mean calls-per-prompt ratio
/// (NOT a percentage — it can exceed 1.0), so render it as a fixed-precision
/// number; `None` (no prompts in the window) renders as `n/a`.
fn format_per_prompt(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |r| format!("{r:.2}"))
}

#[cfg(test)]
mod tests {
    use super::format_per_prompt;

    #[test]
    fn per_prompt_renders_ratio_not_percentage() {
        assert_eq!(format_per_prompt(None), "n/a");
        assert_eq!(format_per_prompt(Some(0.0)), "0.00");
        assert_eq!(format_per_prompt(Some(0.5)), "0.50");
        // Density can exceed 1.0 — must not be clamped or shown as a percent.
        assert_eq!(format_per_prompt(Some(1.5)), "1.50");
    }
}
