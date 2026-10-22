//! StepFun 存量供应商的上下文窗口回填。
//!
//! Step Plan 的 3.x 系列窗口是 262,144 token，但预设早期没有声明窗口和更早的压缩
//! 边界，长会话会在 Claude Code 自动压缩前撞上上游的 400。预设修正只覆盖之后新建的
//! 供应商；库里已有的行仍带着旧配置，所以这里做一次性回填。
//!
//! **不是所有 StepFun 模型都是 256K。** `step-5-preview` 是 1M 窗口 / 64K 输出
//! （platform.stepfun.com 模型页，2026-10 盘点），把它按 256K 钉死会白白丢掉四分之三
//! 的可用上下文。因此回填只在所有模型 env 都指向 256K 模型时才生效：出现
//! `step-5-preview`、或任何带 `[1M]` 标记的模型，整行跳过，留给用户自己声明。
//!
//! 回填只看 provider 行自己的 `settings_config`（不再有运行时的「有效配置」注入层）：
//! 按 `ANTHROPIC_BASE_URL` 认出两个 Step Plan 端点，缺哪个键补哪个。用户已经显式
//! 写过的值保持不动——迁移永远不会覆盖非空的自定义值。

use serde_json::{Map, Value};

use crate::database::Database;
use crate::error::AppError;
use crate::settings::StepFunContextWindowMigration;

/// Step Plan 3.x 的硬上限：262,144 token。
pub(crate) const STEPFUN_CONTEXT_TOKENS: &str = "262144";
/// 提前到 220,000 压缩，给工具结果和 token 估算误差留约 42K 余量。
pub(crate) const STEPFUN_AUTO_COMPACT_WINDOW: &str = "220000";

const APP_TYPE: &str = "claude";

/// Step Plan 的两个区域端点（末尾斜杠忽略）。
const STEPFUN_STEP_PLAN_ENDPOINTS: [&str; 2] = [
    "https://api.stepfun.com/step_plan",
    "https://api.stepfun.ai/step_plan",
];

/// 1M 窗口的 StepFun 模型：不能用 256K 的默认值。
const STEPFUN_ONE_M_MODELS: [&str; 1] = ["step-5-preview"];

/// Claude Code 可能用来选路的所有模型 env。
const CLAUDE_MODEL_ENV_KEYS: [&str; 6] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

#[derive(Debug, Default)]
pub struct StepFunContextWindowOutcome {
    /// 本次补过默认值的供应商 id。
    pub migrated_provider_ids: Vec<String>,
    /// `None` 表示真的跑了；`Some` 是跳过原因。
    pub skipped_reason: Option<String>,
}

/// 启动时调用一次：给存量 StepFun 供应商补上窗口默认值。
pub fn maybe_backfill_stepfun_context_window(
    db: &Database,
) -> Result<StepFunContextWindowOutcome, AppError> {
    if crate::settings::is_stepfun_context_window_migrated() {
        return Ok(StepFunContextWindowOutcome {
            skipped_reason: Some("already_migrated".to_string()),
            ..Default::default()
        });
    }

    let providers = db.get_all_providers(APP_TYPE)?;
    let mut migrated_provider_ids = Vec::new();

    for (_, provider) in &providers {
        let Some(updated) = backfilled_settings(&provider.settings_config) else {
            continue;
        };
        db.update_provider_settings_config(APP_TYPE, &provider.id, &updated)?;
        migrated_provider_ids.push(provider.id.clone());
    }

    crate::settings::mark_stepfun_context_window_migrated(StepFunContextWindowMigration {
        completed_at: chrono::Utc::now().to_rfc3339(),
        migrated_provider_ids: migrated_provider_ids.clone(),
    })?;

    Ok(StepFunContextWindowOutcome {
        migrated_provider_ids,
        skipped_reason: None,
    })
}

/// 纯函数：命中的 StepFun 供应商返回补齐后的配置，否则返回 `None`。
///
/// 只有真的补了至少一个键才返回 `Some`，避免把没变化的行写回库。
fn backfilled_settings(settings: &Value) -> Option<Value> {
    let base_url = settings
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str)
        .map(str::trim)
        .map(|url| url.trim_end_matches('/'))?;
    if !STEPFUN_STEP_PLAN_ENDPOINTS.contains(&base_url) {
        return None;
    }

    let mut updated = settings.clone();
    let env = updated.get_mut("env")?.as_object_mut()?;
    if routes_any_one_m_model(env) {
        return None;
    }
    let mut changed = false;

    for (key, default_value) in [
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", STEPFUN_CONTEXT_TOKENS),
        (
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            STEPFUN_AUTO_COMPACT_WINDOW,
        ),
    ] {
        if missing_or_blank(env, key) {
            env.insert(key.to_string(), Value::String(default_value.to_string()));
            changed = true;
        }
    }

    changed.then_some(updated)
}

/// 缺键或空串视为「用户没设」；数字和任何非空字符串都算显式值。
fn missing_or_blank(env: &Map<String, Value>, key: &str) -> bool {
    match env.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(_) => false,
    }
}

/// 任一模型档位指向 1M 模型时不能用 256K 的默认值：`step-5-preview` 本身，或
/// 任何带 `[1M]` 标记的模型名（Claude Code 以该标记为准，见 `has_one_m_marker`）。
fn routes_any_one_m_model(env: &Map<String, Value>) -> bool {
    CLAUDE_MODEL_ENV_KEYS.iter().any(|key| {
        env.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .is_some_and(|model| {
                model.to_ascii_lowercase().ends_with("[1m]")
                    || STEPFUN_ONE_M_MODELS
                        .iter()
                        .any(|name| model.eq_ignore_ascii_case(name))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stepfun_env(base_url: &str, extra: Value) -> Value {
        let mut env = json!({
            "ANTHROPIC_BASE_URL": base_url,
            "ANTHROPIC_AUTH_TOKEN": "sk-test",
            "ANTHROPIC_MODEL": "step-3.5-flash-2603"
        });
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                env[key.clone()] = value.clone();
            }
        }
        json!({ "env": env })
    }

    #[test]
    fn backfills_both_step_plan_endpoints() {
        for base_url in [
            "https://api.stepfun.com/step_plan",
            "https://api.stepfun.com/step_plan/",
            "https://api.stepfun.ai/step_plan",
        ] {
            let updated = backfilled_settings(&stepfun_env(base_url, json!({})))
                .expect("StepFun provider must be backfilled");
            assert_eq!(
                updated["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],
                json!("262144")
            );
            assert_eq!(
                updated["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
                json!("220000")
            );
        }
    }

    #[test]
    fn leaves_non_stepfun_providers_alone() {
        for base_url in [
            "https://api.stepfun.com/v1",
            "https://api.stepfun.example.com/step_plan",
            "https://api.anthropic.com",
        ] {
            assert!(
                backfilled_settings(&stepfun_env(base_url, json!({}))).is_none(),
                "{base_url} must not be treated as Step Plan"
            );
        }
    }

    #[test]
    fn preserves_explicit_user_values() {
        let updated = backfilled_settings(&stepfun_env(
            "https://api.stepfun.com/step_plan",
            json!({
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "240000",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": 200000
            }),
        ));
        assert!(
            updated.is_none(),
            "a fully configured provider must not be rewritten"
        );
    }

    #[test]
    fn fills_only_the_missing_key() {
        let updated = backfilled_settings(&stepfun_env(
            "https://api.stepfun.ai/step_plan",
            json!({ "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "200000" }),
        ))
        .expect("the missing compact window must be filled");

        assert_eq!(
            updated["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],
            json!("200000"),
            "the user's explicit window wins"
        );
        assert_eq!(
            updated["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
            json!("220000")
        );
    }

    #[test]
    fn treats_blank_strings_as_unset() {
        let updated = backfilled_settings(&stepfun_env(
            "https://api.stepfun.com/step_plan",
            json!({
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "   ",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": ""
            }),
        ))
        .expect("blank values count as unset");

        assert_eq!(
            updated["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],
            json!("262144")
        );
        assert_eq!(
            updated["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
            json!("220000")
        );
    }

    #[test]
    fn ignores_rows_without_env() {
        assert!(backfilled_settings(&json!({ "model": "step-3.5-flash-2603" })).is_none());
        assert!(backfilled_settings(&json!({ "env": [] })).is_none());
    }

    /// step-5-preview 是 1M 窗口：按 256K 回填会白白砍掉四分之三的可用上下文。
    #[test]
    fn skips_step_5_preview() {
        for model in ["step-5-preview", "STEP-5-PREVIEW", " step-5-preview "] {
            let settings = json!({ "env": {
                "ANTHROPIC_BASE_URL": "https://api.stepfun.com/step_plan",
                "ANTHROPIC_MODEL": model,
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "step-5-preview",
            }});
            assert!(
                backfilled_settings(&settings).is_none(),
                "{model} must keep its 1M window"
            );
        }
    }

    /// `[1M]` 标记是 Claude Code 认的窗口声明，回填不能盖过它。
    #[test]
    fn skips_any_tier_with_a_one_m_marker() {
        for key in CLAUDE_MODEL_ENV_KEYS {
            let settings = json!({ "env": {
                "ANTHROPIC_BASE_URL": "https://api.stepfun.ai/step_plan",
                "ANTHROPIC_MODEL": "step-3.7-flash",
                key: "step-3.7-flash[1M]",
            }});
            assert!(
                backfilled_settings(&settings).is_none(),
                "{key} carrying [1M] must suppress the 256K defaults"
            );
        }
    }

    #[test]
    fn still_backfills_a_plain_256k_route() {
        let updated = backfilled_settings(&json!({ "env": {
            "ANTHROPIC_BASE_URL": "https://api.stepfun.com/step_plan",
            "ANTHROPIC_MODEL": "step-3.7-flash",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "step-3.5-flash-2603",
        }}))
        .expect("256K routes must still be backfilled");
        assert_eq!(
            updated["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],
            json!("262144")
        );
    }
}
