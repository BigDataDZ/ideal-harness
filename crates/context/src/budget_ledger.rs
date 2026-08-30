//! D15/TASK-602：由事件流重放根预算、代理 own 用量与子树用量。

use protocol::{ErrorCode, ErrorEnvelope, Event, SequencedEvent};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentTokenUsage {
    pub own_tokens: u64,
    pub subtree_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetLedger {
    root_agent_id: Option<String>,
    token_budget: Option<u64>,
    usages: BTreeMap<String, AgentTokenUsage>,
    usage_ids: BTreeSet<String>,
}

impl BudgetLedger {
    pub fn replay(events: &[SequencedEvent]) -> Result<Self, ErrorEnvelope> {
        let mut ledger = Self::default();
        for record in events {
            ledger.apply(&record.event)?;
        }
        Ok(ledger)
    }

    pub fn apply(&mut self, event: &Event) -> Result<(), ErrorEnvelope> {
        match event {
            Event::TokenBudgetConfigured {
                root_agent_id,
                token_budget,
            } => self.configure(root_agent_id, *token_budget),
            Event::TokenUsageRecorded {
                usage_id,
                agent_path,
                total_tokens,
                ..
            } => self.record(usage_id, agent_path, *total_tokens),
            _ => Ok(()),
        }
    }

    pub fn root_agent_id(&self) -> Option<&str> {
        self.root_agent_id.as_deref()
    }

    pub fn token_budget(&self) -> Option<u64> {
        self.token_budget
    }

    pub fn root_remaining(&self) -> Option<u64> {
        self.token_budget.map(|budget| {
            budget.saturating_sub(
                self.root_agent_id
                    .as_deref()
                    .and_then(|root| self.usages.get(root))
                    .map_or(0, |usage| usage.subtree_tokens),
            )
        })
    }

    pub fn agent_usage(&self, agent_id: &str) -> AgentTokenUsage {
        self.usages.get(agent_id).copied().unwrap_or_default()
    }

    pub fn ensure_can_sample(&self) -> Result<(), ErrorEnvelope> {
        if self.root_remaining() == Some(0) {
            Err(ErrorEnvelope::new(
                ErrorCode::ContextWindowExceeded,
                "root token budget exhausted before model sampling",
            ))
        } else {
            Ok(())
        }
    }

    fn configure(&mut self, root_agent_id: &str, token_budget: u64) -> Result<(), ErrorEnvelope> {
        if root_agent_id.trim().is_empty() || token_budget == 0 {
            return Err(invalid("root agent id and token budget must be non-zero"));
        }
        match (&self.root_agent_id, self.token_budget) {
            (None, None) => {
                self.root_agent_id = Some(root_agent_id.to_string());
                self.token_budget = Some(token_budget);
                Ok(())
            }
            (Some(existing_root), Some(existing_budget))
                if existing_root == root_agent_id && existing_budget == token_budget =>
            {
                Ok(())
            }
            _ => Err(invalid(
                "token budget configuration changed during a session",
            )),
        }
    }

    fn record(
        &mut self,
        usage_id: &str,
        agent_path: &[String],
        total_tokens: u64,
    ) -> Result<(), ErrorEnvelope> {
        validate_usage(usage_id, agent_path)?;
        if !self.usage_ids.insert(usage_id.to_string()) {
            return Err(invalid(format!("duplicate token usage id {usage_id}")));
        }
        let event_root = &agent_path[0];
        match &self.root_agent_id {
            Some(root) if root != event_root => {
                return Err(invalid(
                    "token usage path does not start at configured root",
                ));
            }
            None => self.root_agent_id = Some(event_root.clone()),
            _ => {}
        }

        let leaf = agent_path.last().expect("validated non-empty path");
        checked_add(
            &mut self.usages.entry(leaf.clone()).or_default().own_tokens,
            total_tokens,
        )?;
        for agent in agent_path {
            checked_add(
                &mut self.usages.entry(agent.clone()).or_default().subtree_tokens,
                total_tokens,
            )?;
        }
        Ok(())
    }
}

fn validate_usage(usage_id: &str, agent_path: &[String]) -> Result<(), ErrorEnvelope> {
    if usage_id.trim().is_empty() || agent_path.is_empty() {
        return Err(invalid("usage id and agent path must be non-empty"));
    }
    let mut seen = BTreeSet::new();
    if agent_path
        .iter()
        .any(|agent| agent.trim().is_empty() || !seen.insert(agent))
    {
        return Err(invalid("agent path contains blank or cyclic identities"));
    }
    Ok(())
}

fn checked_add(target: &mut u64, value: u64) -> Result<(), ErrorEnvelope> {
    *target = target
        .checked_add(value)
        .ok_or_else(|| invalid("token usage counter overflow"))?;
    Ok(())
}

fn invalid(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::TokenUsageSource;

    fn se(seq: u64, event: Event) -> SequencedEvent {
        SequencedEvent { seq, event }
    }

    #[test]
    fn two_level_usage_rolls_up_own_subtree_and_remaining() {
        let events = vec![
            se(
                0,
                Event::TokenBudgetConfigured {
                    root_agent_id: "root".into(),
                    token_budget: 100,
                },
            ),
            se(1, usage("root-use", &["root"], 10)),
            se(2, usage("child-use", &["root", "child"], 20)),
            se(3, usage("grand-use", &["root", "child", "grand"], 30)),
        ];
        let ledger = BudgetLedger::replay(&events).unwrap();
        assert_eq!(
            ledger.agent_usage("root"),
            AgentTokenUsage {
                own_tokens: 10,
                subtree_tokens: 60
            }
        );
        assert_eq!(
            ledger.agent_usage("child"),
            AgentTokenUsage {
                own_tokens: 20,
                subtree_tokens: 50
            }
        );
        assert_eq!(
            ledger.agent_usage("grand"),
            AgentTokenUsage {
                own_tokens: 30,
                subtree_tokens: 30
            }
        );
        assert_eq!(ledger.root_remaining(), Some(40));
    }

    #[test]
    fn replay_rejects_duplicate_usage_changed_budget_and_cyclic_path() {
        let configured = se(
            0,
            Event::TokenBudgetConfigured {
                root_agent_id: "root".into(),
                token_budget: 10,
            },
        );
        let first = se(1, usage("same", &["root"], 1));
        let duplicate = se(2, usage("same", &["root"], 1));
        assert!(BudgetLedger::replay(&[configured.clone(), first, duplicate]).is_err());
        assert!(BudgetLedger::replay(&[
            configured.clone(),
            se(
                1,
                Event::TokenBudgetConfigured {
                    root_agent_id: "root".into(),
                    token_budget: 11
                }
            )
        ])
        .is_err());
        assert!(BudgetLedger::replay(&[
            configured,
            se(1, usage("cycle", &["root", "child", "root"], 1))
        ])
        .is_err());
    }

    #[test]
    fn exhausted_budget_rejects_before_another_sample() {
        let events = vec![
            se(
                0,
                Event::TokenBudgetConfigured {
                    root_agent_id: "root".into(),
                    token_budget: 10,
                },
            ),
            se(1, usage("use", &["root"], 12)),
        ];
        let error = BudgetLedger::replay(&events)
            .unwrap()
            .ensure_can_sample()
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ContextWindowExceeded);
    }

    fn usage(id: &str, path: &[&str], total: u64) -> Event {
        Event::TokenUsageRecorded {
            usage_id: id.into(),
            agent_path: path.iter().map(|value| (*value).to_string()).collect(),
            total_tokens: total,
            source: TokenUsageSource::Provider,
        }
    }
}
