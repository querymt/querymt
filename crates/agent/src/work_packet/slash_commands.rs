//! Work-packet slash command plugin and state machine.
//!
//! This module owns all work-packet slash command behavior. The generic slash
//! command core does not know anything about work packets; it just dispatches
//! to plugins. This plugin handles:
//! - /plan
//! - /handoff
//! - /checkpoint
//! - /resume
//! - /status
//! - /packet-show
//! - /packet-list

use crate::model::AgentMessage;
use crate::session::provider::SessionHandle;
use crate::slash_commands::runtime::{
    CommandOutput, RuntimeCommandDescriptor, RuntimeSlashCommandPlugin, SlashCommandExecution,
    SlashCommandHost,
};
use crate::slash_commands::types::SlashCommandInvocation;
use crate::work_packet::brief_generator::{BriefGenerator, BriefGeneratorConfig, BriefMode};
use crate::work_packet::service::{
    PacketResolution, PacketSelector, ResumeOrResolution, WorkPacketService,
};
use crate::work_packet::{CreateWorkPacket, WorkPacket, WorkPacketKind, WorkPacketStore};
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Work-packet command state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum WorkPacketCommand {
    Plan {
        objective: Option<String>,
    },
    Handoff {
        objective: Option<String>,
    },
    Checkpoint {
        objective: Option<String>,
    },
    Resume {
        selector: PacketSelector,
        continue_mode: ContinueMode,
    },
    Status,
    Show {
        selector: PacketSelector,
    },
    List {
        query: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinueMode {
    NoLlm,
    NextStep,
    Continue,
}

#[derive(Clone)]
pub struct WorkPacketSlashPlugin {
    store: Arc<dyn WorkPacketStore>,
    service: WorkPacketService,
}

impl WorkPacketSlashPlugin {
    pub fn new(store: Arc<dyn WorkPacketStore>) -> Self {
        let service = WorkPacketService::new(store.clone());
        Self { store, service }
    }

    async fn execute_machine(
        &self,
        command: WorkPacketCommand,
        host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution {
        match command {
            WorkPacketCommand::Plan { objective } => self.execute_plan(objective, host).await,
            WorkPacketCommand::Handoff { objective } => self.execute_handoff(objective, host).await,
            WorkPacketCommand::Checkpoint { objective } => {
                self.execute_checkpoint(objective, host).await
            }
            WorkPacketCommand::Resume {
                selector,
                continue_mode,
            } => self.execute_resume(selector, continue_mode, host).await,
            WorkPacketCommand::Status => self.execute_status(host).await,
            WorkPacketCommand::Show { selector } => self.execute_show(selector).await,
            WorkPacketCommand::List { query } => self.execute_list(query).await,
        }
    }

    async fn execute_plan(
        &self,
        objective: Option<String>,
        _host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution {
        let command_id = Uuid::new_v4().to_string();
        SlashCommandExecution::Prompt {
            prompt: format_plan_prompt(objective.as_deref()),
            post_turn_action: Some(
                crate::slash_commands::runtime::PostTurnAction::CreatePlanPacket {
                    command_id,
                    command_name: "plan".to_string(),
                    objective,
                },
            ),
        }
    }

    async fn execute_handoff(
        &self,
        objective: Option<String>,
        host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution {
        let history = match host.session_handle().get_effective_agent_history().await {
            Ok(h) => h,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!("Failed to load session history: {}", e)),
                };
            }
        };

        let llm_config = match host.session_handle().llm_config() {
            Some(cfg) => cfg.clone(),
            None => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::text(
                        "No LLM configuration available for this session.".to_string(),
                    ),
                };
            }
        };

        let body = match generate_body(
            host.session_handle(),
            &history,
            objective
                .as_deref()
                .unwrap_or("Create a handoff packet for a future session"),
            BriefMode::Handoff,
            &llm_config,
        )
        .await
        {
            Ok(body) => body,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!("Failed to generate handoff: {}", e)),
                };
            }
        };

        let title = derive_title(WorkPacketKind::Handoff, &body);
        let summary = derive_summary(&body);
        let parent_id = match self.store.get_active_packet(host.session_id()).await {
            Ok(id) => id,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!(
                        "Failed to determine active packet: {}",
                        e
                    )),
                };
            }
        };

        let created = match self
            .store
            .create(CreateWorkPacket {
                scope: host.session_id().to_string(),
                kind: WorkPacketKind::Handoff,
                title,
                summary,
                body_markdown: body,
                metadata_json: None,
                origin_session_id: Some(host.session_id().to_string()),
                parent_packet_id: parent_id.clone(),
                source_delegation_id: None,
                target_delegation_id: None,
            })
            .await
        {
            Ok(pkt) => pkt,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!("Failed to create handoff packet: {}", e)),
                };
            }
        };

        SlashCommandExecution::Handled {
            output: CommandOutput::success(if let Some(parent_id) = parent_id {
                format!(
                    "Created handoff packet `{}` linked to active packet `{}`.\n\nResume later with: `/resume {}`",
                    created.public_id, parent_id, created.public_id
                )
            } else {
                format!(
                    "Created handoff packet `{}`.\n\nResume later with: `/resume {}`",
                    created.public_id, created.public_id
                )
            }),
        }
    }

    async fn execute_checkpoint(
        &self,
        objective: Option<String>,
        host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution {
        let parent_id = match self.store.get_active_packet(host.session_id()).await {
            Ok(id) => id,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!(
                        "Failed to determine active packet: {}",
                        e
                    )),
                };
            }
        };

        let Some(parent_id) = parent_id else {
            return SlashCommandExecution::Handled {
                output: CommandOutput::error(
                    "No active packet. Use `/resume <packet>` first before creating a checkpoint."
                        .to_string(),
                ),
            };
        };

        let history = match host.session_handle().get_effective_agent_history().await {
            Ok(h) => h,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!("Failed to load session history: {}", e)),
                };
            }
        };

        let llm_config = match host.session_handle().llm_config() {
            Some(cfg) => cfg.clone(),
            None => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::text(
                        "No LLM configuration available for this session.".to_string(),
                    ),
                };
            }
        };

        let body = match generate_body(
            host.session_handle(),
            &history,
            objective
                .as_deref()
                .unwrap_or("Create a progress checkpoint"),
            BriefMode::Checkpoint,
            &llm_config,
        )
        .await
        {
            Ok(body) => body,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!("Failed to generate checkpoint: {}", e)),
                };
            }
        };

        let title = derive_title(WorkPacketKind::Checkpoint, &body);
        let summary = derive_summary(&body);

        let created = match self
            .store
            .create(CreateWorkPacket {
                scope: host.session_id().to_string(),
                kind: WorkPacketKind::Checkpoint,
                title,
                summary,
                body_markdown: body,
                metadata_json: None,
                origin_session_id: Some(host.session_id().to_string()),
                parent_packet_id: Some(parent_id.clone()),
                source_delegation_id: None,
                target_delegation_id: None,
            })
            .await
        {
            Ok(pkt) => pkt,
            Err(e) => {
                return SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!(
                        "Failed to create checkpoint packet: {}",
                        e
                    )),
                };
            }
        };

        SlashCommandExecution::Handled {
            output: CommandOutput::success(format!(
                "Created checkpoint packet `{}` linked to active packet `{}`.",
                created.public_id, parent_id
            )),
        }
    }

    async fn execute_resume(
        &self,
        selector: PacketSelector,
        continue_mode: ContinueMode,
        host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution {
        match self
            .service
            .resume_packet(host.session_id(), &selector)
            .await
        {
            Ok(ResumeOrResolution::Resumed(result)) => {
                let output = CommandOutput::markdown(format_resume_response(&result));
                match continue_mode {
                    ContinueMode::Continue => SlashCommandExecution::Hybrid {
                        output: Some(output),
                        prompt: format_continue_prompt(&result.packet),
                        post_turn_action: None,
                    },
                    ContinueMode::NextStep => SlashCommandExecution::Hybrid {
                        output: Some(output),
                        prompt: format_next_step_prompt(&result.packet),
                        post_turn_action: None,
                    },
                    ContinueMode::NoLlm => SlashCommandExecution::Handled { output },
                }
            }
            Ok(ResumeOrResolution::NeedsDisambiguation(PacketResolution::None { query })) => {
                SlashCommandExecution::Handled {
                    output: CommandOutput::error(format!("No packet found for: {}", query)),
                }
            }
            Ok(ResumeOrResolution::NeedsDisambiguation(PacketResolution::Many(packets))) => {
                SlashCommandExecution::Handled {
                    output: CommandOutput::markdown(format_packet_list(
                        "Multiple packets matched",
                        &packets,
                    )),
                }
            }
            Ok(ResumeOrResolution::NeedsDisambiguation(PacketResolution::One(_))) => unreachable!(),
            Err(e) => SlashCommandExecution::Handled {
                output: CommandOutput::error(format!("Error resuming packet: {}", e)),
            },
        }
    }

    async fn execute_status(&self, host: &dyn SlashCommandHost) -> SlashCommandExecution {
        match self.service.active_status(host.session_id()).await {
            Ok(Some(packet)) => SlashCommandExecution::Handled {
                output: CommandOutput::markdown(format!(
                    "**Active packet:** `{}` [{}] {}\n\n**Status:** {}\n**Scope:** {}\n**Updated:** {}\n\n{}",
                    packet.public_id,
                    packet.kind,
                    packet.title,
                    packet.status,
                    packet.scope,
                    packet.updated_at,
                    packet.summary,
                )),
            },
            Ok(None) => SlashCommandExecution::Handled {
                output: CommandOutput::text("No active work packet for this session.".to_string()),
            },
            Err(e) => SlashCommandExecution::Handled {
                output: CommandOutput::error(format!("Error reading active packet: {}", e)),
            },
        }
    }

    async fn execute_show(&self, selector: PacketSelector) -> SlashCommandExecution {
        match self.service.show(&selector).await {
            Ok(PacketResolution::One(pkt)) => SlashCommandExecution::Handled {
                output: CommandOutput::markdown(format_show_packet(&pkt)),
            },
            Ok(PacketResolution::None { query }) => SlashCommandExecution::Handled {
                output: CommandOutput::error(format!("No packet found for: {}", query)),
            },
            Ok(PacketResolution::Many(packets)) => SlashCommandExecution::Handled {
                output: CommandOutput::markdown(format_packet_list(
                    "Multiple packets matched",
                    &packets,
                )),
            },
            Err(e) => SlashCommandExecution::Handled {
                output: CommandOutput::error(format!("Error loading packet: {}", e)),
            },
        }
    }

    async fn execute_list(&self, query: Option<String>) -> SlashCommandExecution {
        match self.service.list(query.as_deref(), 20).await {
            Ok(packets) if packets.is_empty() => SlashCommandExecution::Handled {
                output: CommandOutput::text("No work packets found.".to_string()),
            },
            Ok(packets) => SlashCommandExecution::Handled {
                output: CommandOutput::markdown(format_packet_list("Work packets", &packets)),
            },
            Err(e) => SlashCommandExecution::Handled {
                output: CommandOutput::error(format!("Error listing packets: {}", e)),
            },
        }
    }
}

#[async_trait]
impl RuntimeSlashCommandPlugin for WorkPacketSlashPlugin {
    fn descriptors(&self) -> Vec<RuntimeCommandDescriptor> {
        vec![
            RuntimeCommandDescriptor {
                name: "plan",
                description: "Create a structured plan packet from the current conversation",
                argument_hint: Some("[optional topic focus]"),
            },
            RuntimeCommandDescriptor {
                name: "handoff",
                description: "Create a handoff packet for a future session",
                argument_hint: Some("[optional focus]"),
            },
            RuntimeCommandDescriptor {
                name: "checkpoint",
                description: "Create a progress checkpoint linked to the active packet",
                argument_hint: Some("[optional focus]"),
            },
            RuntimeCommandDescriptor {
                name: "resume",
                description: "Resume a work packet by loading it and setting it as active",
                argument_hint: Some("<packet id or search query>"),
            },
            RuntimeCommandDescriptor {
                name: "status",
                description: "Show the current work packet status for this session",
                argument_hint: None,
            },
            RuntimeCommandDescriptor {
                name: "packet-show",
                description: "Show details of a work packet by id or search query",
                argument_hint: Some("<packet id or search query>"),
            },
            RuntimeCommandDescriptor {
                name: "packet-list",
                description: "List recent work packets or search by query",
                argument_hint: Some("[search query]"),
            },
        ]
    }

    async fn execute(
        &self,
        invocation: &SlashCommandInvocation,
        host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution {
        let command = match invocation.name.as_str() {
            "plan" => WorkPacketCommand::Plan {
                objective: optional_trimmed(&invocation.arguments),
            },
            "handoff" => WorkPacketCommand::Handoff {
                objective: optional_trimmed(&invocation.arguments),
            },
            "checkpoint" => WorkPacketCommand::Checkpoint {
                objective: optional_trimmed(&invocation.arguments),
            },
            "resume" => {
                let (selector_str, continue_mode) =
                    parse_continue_flags(invocation.arguments.trim());
                WorkPacketCommand::Resume {
                    selector: PacketSelector::from_arg(selector_str),
                    continue_mode,
                }
            }
            "status" => WorkPacketCommand::Status,
            "packet-show" => WorkPacketCommand::Show {
                selector: PacketSelector::from_arg(&invocation.arguments),
            },
            "packet-list" => WorkPacketCommand::List {
                query: optional_trimmed(&invocation.arguments),
            },
            _ => return SlashCommandExecution::NotHandled,
        };

        self.execute_machine(command, host).await
    }
}

fn optional_trimmed(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_continue_flags(args: &str) -> (&str, ContinueMode) {
    if args.contains("--continue") {
        let end = args.find("--continue").unwrap_or(args.len());
        (args[..end].trim(), ContinueMode::Continue)
    } else if args.contains("--next") {
        let end = args.find("--next").unwrap_or(args.len());
        (args[..end].trim(), ContinueMode::NextStep)
    } else {
        (args, ContinueMode::NoLlm)
    }
}

async fn generate_body(
    session_handle: &SessionHandle,
    history: &[AgentMessage],
    objective: &str,
    mode: BriefMode,
    llm_config: &crate::session::store::LLMConfig,
) -> Result<String, String> {
    let provider = match session_handle.provider().await {
        Ok(provider) => provider,
        Err(e) => return Err(format!("failed to construct provider: {}", e)),
    };

    let config = BriefGeneratorConfig {
        provider: llm_config.provider.clone(),
        model: llm_config.model.clone(),
        api_key: None,
        max_tokens: None,
        timeout_secs: 60,
        min_history_tokens: 0,
    };

    let generator = BriefGenerator::from_parts(
        provider,
        std::time::Duration::from_secs(config.timeout_secs),
        config.min_history_tokens,
    );

    generator
        .generate(history, objective, mode)
        .await
        .map_err(|e| e.to_string())
}

fn format_plan_prompt(objective: Option<&str>) -> String {
    let focus = objective
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("current discussion");

    format!(
        "The user invoked /plan.\n\nCreate a durable implementation plan from the current conversation and the user's focus:\n{focus}\n\nUse the full session context. Preserve concrete details, decisions, filenames, constraints, rejected alternatives, and open questions. Do not compress the conversation into a generic brief.\n\nWrite the plan as markdown suitable for saving as a Work Packet.\n\nInclude:\n- Goal\n- Current understanding\n- Relevant files/modules\n- Key decisions and rationale\n- Rejected alternatives\n- Implementation phases\n- Risks and tradeoffs\n- Verification strategy\n- Open questions\n\nIf essential information is missing, ask concise clarifying questions instead of inventing details."
    )
}

pub(crate) fn derive_title(kind: WorkPacketKind, body: &str) -> String {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }

    for line in body.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return trimmed.chars().take(80).collect();
        }
    }

    format!("{} packet", kind)
}

pub(crate) fn derive_summary(body: &str) -> String {
    let text = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");

    let summary: String = text.chars().take(180).collect();
    if summary.is_empty() {
        "No summary available.".to_string()
    } else {
        summary
    }
}

fn format_resume_response(result: &crate::work_packet::service::ResumePacketResult) -> String {
    let pkt = &result.packet;
    format!(
        "**Resumed packet:** `{}` [{}] {}\n\n**Status:** {} → {}\n**Scope:** {}\n**Updated:** {}\n\n{}\n\nUse `/checkpoint` to save progress.\nUse `/status` to check current state.",
        pkt.public_id,
        pkt.kind,
        pkt.title,
        result.previous_status,
        pkt.status,
        pkt.scope,
        pkt.updated_at,
        pkt.summary,
    )
}

fn format_packet_list(header: &str, packets: &[WorkPacket]) -> String {
    let mut lines = vec![format!("**{} ({}):**\n", header, packets.len())];
    for pkt in packets {
        lines.push(format!(
            "- `{}` [{}] {} — *{}* (status: {})",
            pkt.public_id, pkt.kind, pkt.title, pkt.summary, pkt.status
        ));
    }
    lines.join("\n")
}

fn format_show_packet(pkt: &WorkPacket) -> String {
    let mut out = format!(
        "# {} [{}]\n\n**ID:** {}\n**Status:** {}\n**Scope:** {}\n**Kind:** {}\n**Created:** {}\n**Updated:** {}\n",
        pkt.title,
        pkt.kind,
        pkt.public_id,
        pkt.status,
        pkt.scope,
        pkt.kind,
        pkt.created_at,
        pkt.updated_at,
    );
    if let Some(ref parent) = pkt.parent_packet_id {
        out.push_str(&format!("**Parent:** {}\n", parent));
    }
    if let Some(ref session) = pkt.origin_session_id {
        out.push_str(&format!("**Origin session:** {}\n", session));
    }
    out.push_str(&format!("\n## Summary\n\n{}\n", pkt.summary));
    out.push_str(&format!("\n## Body\n\n{}\n", pkt.body_markdown));
    out
}

fn format_continue_prompt(packet: &WorkPacket) -> String {
    format!(
        "The user resumed work packet `{}` and wants to continue working on it.\n\n**Packet:** {} [{}] — {}\n**Status:** {}\n\n## Summary\n\n{}\n\n## Body\n\n{}\n\nContinue implementing the next logical step. Do not attempt the entire plan at once. Record progress using `/checkpoint`.",
        packet.public_id,
        packet.title,
        packet.kind,
        packet.summary,
        packet.status,
        packet.summary,
        packet.body_markdown,
    )
}

fn format_next_step_prompt(packet: &WorkPacket) -> String {
    format!(
        "The user resumed work packet `{}` and wants to identify the next step.\n\n**Packet:** {} [{}] — {}\n**Status:** {}\n\n## Summary\n\n{}\n\n## Body\n\n{}\n\nRead the packet body above and identify ONLY the next unfinished phase or next recommended action. Create todos for that phase only. Do not attempt to execute the entire plan.",
        packet.public_id,
        packet.title,
        packet.kind,
        packet.summary,
        packet.status,
        packet.summary,
        packet.body_markdown,
    )
}
