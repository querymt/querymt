use std::fmt;

pub const NAME: &str = "delegate";
pub const DESCRIPTION: &str = "Delegate a task to a profile or registered agent";
pub const ARGUMENT_HINT: &str = "[--wait|--async] <profile:id|agent:id> <task>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateMode {
    Wait,
    Async,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateTarget {
    Profile(String),
    Agent(String),
}

impl DelegateTarget {
    pub fn id(&self) -> &str {
        match self {
            Self::Profile(id) | Self::Agent(id) => id,
        }
    }
}

impl fmt::Display for DelegateTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(id) => write!(f, "profile:{id}"),
            Self::Agent(id) => write!(f, "agent:{id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateSlashCommand {
    pub mode: DelegateMode,
    pub target: DelegateTarget,
    pub objective: String,
}

pub fn parse_delegate_command(arguments: &str) -> Result<DelegateSlashCommand, String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Err(usage("missing target and task"));
    }

    let mut mode = None;
    let mut offset = 0;
    let mut target_token = None;

    for token in trimmed.split_whitespace() {
        let token_start = trimmed[offset..]
            .find(token)
            .map(|index| offset + index)
            .unwrap_or(offset);
        offset = token_start + token.len();

        match token {
            "--wait" => set_mode(&mut mode, DelegateMode::Wait)?,
            "--async" => set_mode(&mut mode, DelegateMode::Async)?,
            value if value.starts_with("--") => {
                return Err(usage(&format!("unknown option '{value}'")));
            }
            value => {
                target_token = Some((value, offset));
                break;
            }
        }
    }

    let Some((target_token, target_end)) = target_token else {
        return Err(usage("missing target"));
    };
    let target = parse_target(target_token)?;
    let objective = trimmed[target_end..].trim().to_string();
    if objective.is_empty() {
        return Err(usage("missing task"));
    }

    Ok(DelegateSlashCommand {
        mode: mode.unwrap_or(DelegateMode::Wait),
        target,
        objective,
    })
}

fn set_mode(mode: &mut Option<DelegateMode>, requested: DelegateMode) -> Result<(), String> {
    if let Some(existing) = mode {
        if *existing == requested {
            return Err(usage("delegation mode was specified more than once"));
        }
        return Err(usage("--wait and --async cannot be used together"));
    }
    *mode = Some(requested);
    Ok(())
}

fn parse_target(value: &str) -> Result<DelegateTarget, String> {
    let (kind, id) = value
        .split_once(':')
        .ok_or_else(|| usage("target must use the profile: or agent: prefix"))?;
    if id.trim().is_empty() {
        return Err(usage("target id cannot be empty"));
    }
    match kind {
        "profile" => Ok(DelegateTarget::Profile(id.to_string())),
        "agent" => Ok(DelegateTarget::Agent(id.to_string())),
        _ => Err(usage("target must use the profile: or agent: prefix")),
    }
}

fn usage(message: &str) -> String {
    format!("{message}. Usage: /{NAME} {ARGUMENT_HINT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profile_wait_by_default() {
        let command = parse_delegate_command("profile:reviewer Review this branch").unwrap();
        assert_eq!(command.mode, DelegateMode::Wait);
        assert_eq!(command.target, DelegateTarget::Profile("reviewer".into()));
        assert_eq!(command.objective, "Review this branch");
    }

    #[test]
    fn parses_async_remote_agent_and_multiline_task() {
        let command = parse_delegate_command(
            "--async agent:gpu-coder Implement the fix\nand run the test suite",
        )
        .unwrap();
        assert_eq!(command.mode, DelegateMode::Async);
        assert_eq!(command.target, DelegateTarget::Agent("gpu-coder".into()));
        assert_eq!(
            command.objective,
            "Implement the fix\nand run the test suite"
        );
    }

    #[test]
    fn rejects_ambiguous_and_conflicting_inputs() {
        assert!(parse_delegate_command("reviewer Review").is_err());
        assert!(parse_delegate_command("--wait --async profile:reviewer Review").is_err());
        assert!(parse_delegate_command("profile:reviewer").is_err());
        assert!(parse_delegate_command("peer:reviewer Review").is_err());
    }
}
