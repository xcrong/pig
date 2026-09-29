use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    slash_meta! {
        name: "login",
        description: "Show how to configure a model provider (browser login removed)",
        usage: "/login",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Message(
            "Browser login was removed. Configure a model provider instead: set `[vendors.<id>] enabled = true` with its `env_key`, or add `[model_providers.*]` (see docs/user-guide/11-custom-models.md)."
                .to_string(),
        )
    }
}
