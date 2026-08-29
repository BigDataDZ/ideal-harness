//! P4/TASK-401：显式 resume / fork 会话命令。

use crate::{cmd_chat, parse_chat_args, ChatArgs};
use session::{fork as session_fork, replay_session};
use std::path::PathBuf;

pub(crate) fn cmd_resume(args: &[String]) -> anyhow::Result<()> {
    let config = parse_resume_args(args)?;
    cmd_chat(&chat_args(&config))
}

fn parse_resume_args(args: &[String]) -> anyhow::Result<ChatArgs> {
    if !args.iter().any(|arg| arg == "--session") {
        anyhow::bail!("resume 必须显式提供 --session <path>");
    }
    let config = parse_chat_args(args)?;
    if !config.session.is_file() {
        anyhow::bail!("待恢复会话不存在：{}", config.session.display());
    }
    replay_session(&config.session)?;
    Ok(config)
}

fn chat_args(config: &ChatArgs) -> Vec<String> {
    vec![
        "--session".into(),
        config.session.to_string_lossy().into_owned(),
        "--base-url".into(),
        config.base_url.clone(),
        "--model".into(),
        config.model.clone(),
    ]
}

struct ForkArgs {
    source: PathBuf,
    target: PathBuf,
    boundary: Option<usize>,
}

fn parse_fork_args(args: &[String]) -> anyhow::Result<ForkArgs> {
    let mut source = None;
    let mut target = None;
    let mut boundary = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("{flag} 缺少取值"))?;
        match flag {
            "--session" => source = Some(PathBuf::from(value)),
            "--target" => target = Some(PathBuf::from(value)),
            "--boundary" => {
                boundary = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| anyhow::anyhow!("--boundary 必须是非负整数"))?,
                )
            }
            other => anyhow::bail!("未知 fork 参数：{other}"),
        }
        index += 2;
    }
    Ok(ForkArgs {
        source: source.ok_or_else(|| anyhow::anyhow!("fork 缺少 --session <source>"))?,
        target: target.ok_or_else(|| anyhow::anyhow!("fork 缺少 --target <path>"))?,
        boundary,
    })
}

pub(crate) fn cmd_fork(args: &[String]) -> anyhow::Result<()> {
    let config = parse_fork_args(args)?;
    if !config.source.is_file() {
        anyhow::bail!("源会话不存在：{}", config.source.display());
    }
    if config.target.exists() {
        anyhow::bail!("目标会话已存在，拒绝覆盖：{}", config.target.display());
    }
    let source_events = replay_session(&config.source)?;
    let boundary = config.boundary.unwrap_or(source_events.len());
    if boundary > source_events.len() {
        anyhow::bail!(
            "fork boundary {boundary} 超出源事件数 {}",
            source_events.len()
        );
    }
    let child = session_fork(&config.source, config.target.clone(), boundary)?;
    println!(
        "已 fork {} 个事件：{} → {}",
        child.len(),
        config.source.display(),
        config.target.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Event;
    use session::JsonlSession;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-cli-session-{}-{name}", std::process::id()))
    }

    #[test]
    fn resume_requires_existing_valid_explicit_session() {
        assert!(parse_resume_args(&[]).is_err());
        let missing = tmp("missing.jsonl");
        let _ = std::fs::remove_file(&missing);
        assert!(
            parse_resume_args(&["--session".into(), missing.to_string_lossy().into()]).is_err()
        );

        let valid = tmp("valid.jsonl");
        let _ = std::fs::remove_file(&valid);
        let mut session = JsonlSession::create(valid.clone()).unwrap();
        session
            .append(Event::UserMessage { text: "hi".into() })
            .unwrap();
        let parsed =
            parse_resume_args(&["--session".into(), valid.to_string_lossy().into()]).unwrap();
        assert_eq!(parsed.session, valid);
        std::fs::remove_file(valid).ok();
    }

    #[test]
    fn forked_sessions_append_independently() {
        let source = tmp("source.jsonl");
        let target = tmp("target.jsonl");
        let _ = (std::fs::remove_file(&source), std::fs::remove_file(&target));
        let mut parent = JsonlSession::create(source.clone()).unwrap();
        parent
            .append(Event::UserMessage {
                text: "seed".into(),
            })
            .unwrap();
        parent
            .append(Event::AssistantMessage {
                text: "answer".into(),
            })
            .unwrap();
        drop(parent);

        cmd_fork(&[
            "--session".into(),
            source.to_string_lossy().into(),
            "--target".into(),
            target.to_string_lossy().into(),
        ])
        .unwrap();
        let mut parent = JsonlSession::create(source.clone()).unwrap();
        let mut child = JsonlSession::create(target.clone()).unwrap();
        parent
            .append(Event::UserMessage {
                text: "parent".into(),
            })
            .unwrap();
        child
            .append(Event::UserMessage {
                text: "child".into(),
            })
            .unwrap();

        assert_eq!(
            replay_session(&source).unwrap().last().unwrap().event,
            Event::UserMessage {
                text: "parent".into()
            }
        );
        assert_eq!(
            replay_session(&target).unwrap().last().unwrap().event,
            Event::UserMessage {
                text: "child".into()
            }
        );
        assert!(cmd_fork(&[
            "--session".into(),
            source.to_string_lossy().into(),
            "--target".into(),
            target.to_string_lossy().into(),
        ])
        .is_err());
        let _ = (std::fs::remove_file(source), std::fs::remove_file(target));
    }

    #[test]
    fn fork_boundary_is_validated() {
        let parsed = parse_fork_args(&[
            "--session".into(),
            "a".into(),
            "--target".into(),
            "b".into(),
            "--boundary".into(),
            "2".into(),
        ])
        .unwrap();
        assert_eq!(parsed.boundary, Some(2));
        assert!(parse_fork_args(&["--boundary".into(), "x".into()]).is_err());
    }
}
