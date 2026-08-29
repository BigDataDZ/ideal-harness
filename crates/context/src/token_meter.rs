//! P3/TASK-301：provider usage 优先、启发式兜底的 token 计量器。

use crate::TokenUsage;

/// 本次计量采用的数据来源，供压缩决策和诊断区分真实值与估算值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    ProviderUsage,
    Heuristic,
}

/// 一次 token 计量结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenMeasurement {
    pub usage: TokenUsage,
    pub source: TokenSource,
}

/// Token 计量器。provider usage 存在时是唯一锚点；缺失时才估算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenMeter {
    chars_per_token: u64,
}

impl Default for TokenMeter {
    fn default() -> Self {
        Self { chars_per_token: 4 }
    }
}

impl TokenMeter {
    /// 创建启发式计量器。零字符/token 没有定义，必须拒绝。
    pub fn new(chars_per_token: u64) -> Result<Self, &'static str> {
        if chars_per_token == 0 {
            return Err("chars_per_token must be greater than zero");
        }
        Ok(Self { chars_per_token })
    }

    /// 计量一组模型可见文本。
    ///
    /// `provider_total=Some(0)` 也是可信 usage，不会因值为零误走兜底。
    pub fn measure(
        &self,
        provider_total: Option<u64>,
        visible_segments: &[&str],
    ) -> TokenMeasurement {
        match provider_total {
            Some(total) => TokenMeasurement {
                usage: TokenUsage { total },
                source: TokenSource::ProviderUsage,
            },
            None => TokenMeasurement {
                usage: TokenUsage {
                    total: self.estimate(visible_segments),
                },
                source: TokenSource::Heuristic,
            },
        }
    }

    fn estimate(&self, visible_segments: &[&str]) -> u64 {
        let characters = visible_segments.iter().fold(0u64, |total, segment| {
            let count = u64::try_from(segment.chars().count()).unwrap_or(u64::MAX);
            total.saturating_add(count)
        });
        characters / self.chars_per_token + u64::from(characters % self.chars_per_token != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_usage_is_exact_anchor_even_when_zero() {
        let meter = TokenMeter::default();
        for total in [0, 37, u64::MAX] {
            let measured = meter.measure(Some(total), &["this text must be ignored"]);
            assert_eq!(measured.usage.total, total);
            assert_eq!(measured.source, TokenSource::ProviderUsage);
        }
    }

    #[test]
    fn missing_usage_falls_back_to_rounded_up_character_estimate() {
        let meter = TokenMeter::new(4).unwrap();
        let measured = meter.measure(None, &["abc", "你好"]);
        assert_eq!(measured.usage.total, 2, "5 个字符按 4:1 向上取整");
        assert_eq!(measured.source, TokenSource::Heuristic);
    }

    #[test]
    fn empty_context_estimates_zero_and_nonempty_never_estimates_zero() {
        let meter = TokenMeter::default();
        assert_eq!(meter.measure(None, &[]).usage.total, 0);
        assert_eq!(meter.measure(None, &[""]).usage.total, 0);
        assert_eq!(meter.measure(None, &["x"]).usage.total, 1);
    }

    #[test]
    fn invalid_heuristic_ratio_is_rejected() {
        assert_eq!(
            TokenMeter::new(0).unwrap_err(),
            "chars_per_token must be greater than zero"
        );
    }
}
