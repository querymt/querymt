use crate::commands::{CommandResult, SlashCommand};
use anyhow::Result;
use async_trait::async_trait;

/// Clear command - clears the screen
pub struct ClearCommand;

#[async_trait]
impl SlashCommand for ClearCommand {
    fn name(&self) -> &str {
        "clear"
    }

    fn description(&self) -> &str {
        "Clear the screen"
    }

    fn usage(&self) -> &str {
        ""
    }

    fn execute(&self, _args: Vec<String>) -> Result<CommandResult> {
        // ANSI escape code to clear screen
        print!("\x1B[2J\x1B[1;1H");
        Ok(CommandResult::Success(String::new()))
    }
}

/// Exit command - exits the application
pub struct ExitCommand;

#[async_trait]
impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "exit"
    }

    fn description(&self) -> &str {
        "Exit the application"
    }

    fn usage(&self) -> &str {
        ""
    }

    fn execute(&self, _args: Vec<String>) -> Result<CommandResult> {
        Ok(CommandResult::Exit)
    }
}
