use crate::api::schema::conversations::{
    AgentConversationReportParams, AgentConversationRespondParams, ConversationAvailability,
    ConversationCapability, ConversationPage, ConversationReadResult, ConversationReasonCode,
    ConversationRespondReason,
};
use crate::api::schema::{AgentConversationReadParams, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

impl App {
    pub(super) fn apply_conversation_metadata_patch(
        &mut self,
        pane_id: crate::layout::PaneId,
        patch: std::collections::HashMap<String, Option<String>>,
    ) {
        if patch.is_empty() {
            return;
        }
        let Some((ws_idx, pane)) = self.find_pane(pane_id) else {
            return;
        };
        let terminal_id = pane.attached_terminal_id.clone();
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return;
        };
        if terminal
            .metadata_tokens
            .patch(patch, None, std::time::Instant::now())
        {
            terminal.revision = terminal.revision.saturating_add(1);
            self.emit_pane_updated(ws_idx, pane_id);
        }
    }

    pub(super) fn handle_agent_conversation_read(
        &mut self,
        id: String,
        params: AgentConversationReadParams,
    ) -> String {
        self.handle_agent_conversation_read_internal(id, params, false)
    }

    pub(super) fn handle_agent_conversation_metadata(
        &mut self,
        id: String,
        params: AgentConversationReadParams,
    ) -> String {
        self.handle_agent_conversation_read_internal(id, params, true)
    }

    fn handle_agent_conversation_read_internal(
        &mut self,
        id: String,
        params: AgentConversationReadParams,
        metadata_only: bool,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(error) => return encode_error_body(id, self.agent_target_error_body(error)),
        };
        let Some(workspace) = self.state.workspaces.get(resolved.ws_idx) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(pane_state) = workspace.pane_state(resolved.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(terminal) = self.state.terminals.get(&pane_state.attached_terminal_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let display_root = terminal.cwd.clone();
        let Some(agent) = terminal.effective_agent_label() else {
            return encode_error(
                id,
                "conversation_no_session",
                "structured Chat requires an active provider session",
            );
        };
        let Some(provider) = provider_for_label(agent) else {
            return encode_error(
                id,
                "conversation_unsupported",
                "structured Chat is not supported for this provider",
            );
        };
        let Some(session_parts) =
            super::super::creation::terminal_effective_session_parts(terminal)
        else {
            return encode_error(
                id,
                "conversation_no_session",
                "structured Chat requires an active provider session",
            );
        };
        let session_key = crate::app::conversation_sources::session_identity_key(
            &session_parts.0,
            &session_parts.1,
            session_parts.2.kind_name(),
            &session_parts.3,
        );
        let Some(source_entry) = self
            .state
            .conversation_sources
            .current_for(resolved.pane_id, Some(&session_key))
        else {
            return encode_error(
                id,
                "conversation_transcript_missing",
                "the provider transcript is not available yet",
            );
        };
        let Some(transcript) = source_entry.transcript_ref().cloned() else {
            return encode_error(
                id,
                "conversation_transcript_missing",
                "the provider transcript is not available yet",
            );
        };
        let Some(conversation_id) = source_entry.conversation_handle().map(str::to_string) else {
            return encode_error(
                id,
                "conversation_transcript_missing",
                "the provider transcript is not available yet",
            );
        };

        self.sync_conversation_cache_for_pane(resolved.pane_id);
        let current_tokens = self
            .find_pane(resolved.pane_id)
            .and_then(|(_, pane)| self.state.terminals.get(&pane.attached_terminal_id))
            .map(|terminal| terminal.metadata_tokens.values())
            .unwrap_or_default();
        let reader = self
            .conversation_readers
            .entry(resolved.pane_id)
            .or_insert_with(|| {
                crate::agent_conversation::ConversationReader::new(
                    provider,
                    conversation_id.clone(),
                    &conversation_id,
                    1,
                )
            });
        if reader.session_id() != conversation_id {
            *reader = crate::agent_conversation::ConversationReader::new(
                provider,
                conversation_id.clone(),
                &conversation_id,
                1,
            );
        }
        reader.set_display_root(&display_root);
        let outcome = if metadata_only {
            reader.read_metadata(&transcript)
        } else {
            reader.read(
                &transcript,
                params.cursor.as_deref(),
                params.direction,
                params.limit,
            )
        };
        let metadata_patch = reader.take_metadata_patch(
            outcome.reset || outcome.capability_reason.is_some(),
            &current_tokens,
        );
        self.apply_conversation_metadata_patch(resolved.pane_id, metadata_patch);
        if outcome.reset {
            return encode_success(
                id,
                ResponseResult::AgentConversationRead {
                    read: ConversationReadResult::ResetRequired {
                        session: outcome.session,
                        reader_generation: outcome.generation,
                    },
                },
            );
        }
        if let Some(reason) = outcome.capability_reason {
            return encode_error(id, reason_code(reason), safe_reason_message(reason));
        }
        let Some(page) = outcome.page else {
            return encode_error(
                id,
                "conversation_unavailable",
                "structured Chat is temporarily unavailable",
            );
        };
        let page = ConversationPage {
            provider: provider_label(provider).into(),
            session: outcome.session,
            capability: ConversationCapability {
                availability: ConversationAvailability::Supported,
                reason: ConversationReasonCode::Ready,
                message: None,
            },
            items: page.items,
            next_cursor: page.next_cursor,
            previous_cursor: page.previous_cursor,
            has_older: page.has_older,
            revision: page.revision,
            reader_generation: outcome.generation,
        };
        encode_success(
            id,
            ResponseResult::AgentConversationRead {
                read: ConversationReadResult::Page { page },
            },
        )
    }
    pub(super) fn handle_agent_conversation_report(
        &mut self,
        id: String,
        params: AgentConversationReportParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id).cloned())
        else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let token_ok = self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|terminal| terminal.integration_token.as_deref())
            .is_some_and(|expected| expected == params.integration_token);
        if !token_ok {
            return encode_error(
                id,
                "invalid_integration_token",
                "conversation report rejected: missing or stale integration token",
            );
        }
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(agent) = super::super::api_helpers::normalize_reported_agent_label(&params.agent)
        else {
            return encode_error(id, "invalid_agent", "invalid agent label");
        };
        if terminal.effective_agent_label() != Some(agent.as_str()) {
            return encode_error(
                id,
                "conversation_session_mismatch",
                "conversation report belongs to a different active provider session",
            );
        }
        if !matches!(agent.as_str(), "pi" | "omp" | "codex" | "claude") {
            return encode_error(
                id,
                "conversation_unsupported",
                "structured Chat is not supported for this provider",
            );
        }
        let Some(session_ref) = crate::agent_resume::session_ref_from_report(
            &params.source,
            &agent,
            params.agent_session_id.clone(),
            params.agent_session_path.clone(),
        ) else {
            return encode_error(
                id,
                "conversation_session_mismatch",
                "conversation report is missing its active provider session",
            );
        };
        let Some(current_parts) =
            super::super::creation::terminal_effective_session_parts(terminal)
        else {
            return encode_error(
                id,
                "conversation_no_session",
                "structured Chat requires an active provider session",
            );
        };
        let current_identity = crate::app::conversation_sources::session_identity_key(
            &current_parts.0,
            &current_parts.1,
            current_parts.2.kind_name(),
            &current_parts.3,
        );
        let reported_identity = crate::app::conversation_sources::session_identity_key(
            &params.source,
            &agent,
            session_ref.kind.kind_name(),
            &session_ref.value,
        );
        if current_identity != reported_identity {
            return encode_error(
                id,
                "conversation_session_mismatch",
                "conversation report belongs to a different active provider session",
            );
        }
        if !self.accept_conversation_overlay(
            pane_id,
            params.seq,
            params.native_id,
            params.entry_id,
            params.turn_id,
            params.timestamp_ms,
            params.payload,
        ) {
            return encode_error(
                id,
                "conversation_report_rejected",
                "conversation live item exceeded the engine limits or was invalid",
            );
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_agent_conversation_respond(
        &mut self,
        id: String,
        params: AgentConversationRespondParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(error) => return encode_error_body(id, self.agent_target_error_body(error)),
        };
        let Some(workspace) = self.state.workspaces.get(resolved.ws_idx) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(pane_state) = workspace.pane_state(resolved.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(terminal) = self.state.terminals.get(&pane_state.attached_terminal_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let display_root = terminal.cwd.clone();
        let Some(provider_name) = terminal.effective_agent_label() else {
            return encode_error(
                id,
                "conversation_no_session",
                "structured Chat requires an active provider session",
            );
        };
        let Some(provider) = provider_for_label(provider_name) else {
            return encode_error(
                id,
                "conversation_unsupported",
                "structured Chat is not supported for this provider",
            );
        };
        let Some(parts) = super::super::creation::terminal_effective_session_parts(terminal) else {
            return encode_error(
                id,
                "conversation_no_session",
                "structured Chat requires an active provider session",
            );
        };
        let session_key = crate::app::conversation_sources::session_identity_key(
            &parts.0,
            &parts.1,
            parts.2.kind_name(),
            &parts.3,
        );
        let Some(source_entry) = self
            .state
            .conversation_sources
            .current_for(resolved.pane_id, Some(&session_key))
        else {
            return encode_error(
                id,
                "conversation_transcript_missing",
                "the provider transcript is not available yet",
            );
        };
        let Some(transcript) = source_entry.transcript_ref().cloned() else {
            return encode_error(
                id,
                "conversation_transcript_missing",
                "the provider transcript is not available yet",
            );
        };
        let Some(conversation_id) = source_entry.conversation_handle().map(str::to_string) else {
            return encode_error(
                id,
                "conversation_transcript_missing",
                "the provider transcript is not available yet",
            );
        };
        if params.session.id != conversation_id {
            return respond_result(
                id,
                &params,
                false,
                ConversationRespondReason::SessionMismatch,
            );
        }

        let reader = self
            .conversation_readers
            .entry(resolved.pane_id)
            .or_insert_with(|| {
                crate::agent_conversation::ConversationReader::new(
                    provider,
                    conversation_id.clone(),
                    &conversation_id,
                    1,
                )
            });
        if reader.session_id() != conversation_id {
            *reader = crate::agent_conversation::ConversationReader::new(
                provider,
                conversation_id.clone(),
                &conversation_id,
                1,
            );
        }
        reader.set_display_root(&display_root);
        let outcome = reader.read(
            &transcript,
            None,
            crate::api::schema::ConversationPageDirection::Newest,
            crate::api::schema::CONVERSATION_MAX_LIMIT,
        );
        if outcome.reset || outcome.generation != params.reader_generation {
            return respond_result(
                id,
                &params,
                false,
                ConversationRespondReason::SessionMismatch,
            );
        }
        let Some(page) = outcome.page else {
            return respond_result(
                id,
                &params,
                false,
                ConversationRespondReason::UnknownRequest,
            );
        };
        let Some((status, structured_response, decision_allowed, selected_decision)) =
            page.items.iter().find_map(|item| match &item.payload {
                crate::api::schema::ConversationItemPayload::Approval {
                    request_id,
                    status,
                    decisions,
                    structured_response,
                    selected_decision,
                    ..
                } if request_id == &params.request_id => Some((
                    *status,
                    *structured_response,
                    decisions
                        .iter()
                        .any(|decision| decision.id == params.decision_id),
                    selected_decision.as_deref(),
                )),
                _ => None,
            })
        else {
            return respond_result(
                id,
                &params,
                false,
                ConversationRespondReason::UnknownRequest,
            );
        };
        if status == crate::api::schema::ApprovalStatus::Resolved {
            return respond_result(
                id,
                &params,
                selected_decision == Some(params.decision_id.as_str()),
                ConversationRespondReason::AlreadyResolved,
            );
        }
        if !decision_allowed {
            return respond_result(
                id,
                &params,
                false,
                ConversationRespondReason::ConflictingDecision,
            );
        }
        if !structured_response {
            return encode_error(
                id,
                "conversation_approval_unsupported",
                "this provider approval must be answered in Terminal",
            );
        }
        let _ = provider;
        encode_error(
            id,
            "conversation_approval_unsupported",
            "this provider has no safe structured approval responder",
        )
    }
}
fn respond_result(
    id: String,
    params: &AgentConversationRespondParams,
    accepted: bool,
    reason: ConversationRespondReason,
) -> String {
    encode_success(
        id,
        ResponseResult::AgentConversationRespond {
            result: crate::api::schema::ConversationRespondResult {
                request_id: params.request_id.clone(),
                decision_id: params.decision_id.clone(),
                accepted,
                reason,
            },
        },
    )
}

fn provider_for_label(label: &str) -> Option<crate::detect::Agent> {
    match label {
        "pi" => Some(crate::detect::Agent::Pi),
        "omp" => Some(crate::detect::Agent::Omp),
        "codex" => Some(crate::detect::Agent::Codex),
        "claude" => Some(crate::detect::Agent::Claude),
        _ => None,
    }
}

fn provider_label(provider: crate::detect::Agent) -> &'static str {
    match provider {
        crate::detect::Agent::Pi => "pi",
        crate::detect::Agent::Omp => "omp",
        crate::detect::Agent::Codex => "codex",
        crate::detect::Agent::Claude => "claude",
        _ => "unknown",
    }
}

fn reason_code(reason: ConversationReasonCode) -> &'static str {
    match reason {
        ConversationReasonCode::TranscriptMissing => "conversation_transcript_missing",
        ConversationReasonCode::TranscriptInvalid => "conversation_transcript_invalid",
        ConversationReasonCode::SourceUnreadable => "conversation_source_unreadable",
        ConversationReasonCode::NoSession => "conversation_no_session",
        ConversationReasonCode::AdapterMissing => "conversation_unsupported",
        ConversationReasonCode::Ready => "conversation_ready",
    }
}

fn safe_reason_message(reason: ConversationReasonCode) -> &'static str {
    match reason {
        ConversationReasonCode::TranscriptMissing => "the provider transcript is not available yet",
        ConversationReasonCode::TranscriptInvalid => {
            "the provider transcript is not valid for this provider"
        }
        ConversationReasonCode::SourceUnreadable => {
            "the provider transcript is temporarily unreadable"
        }
        ConversationReasonCode::NoSession => "structured Chat requires an active provider session",
        ConversationReasonCode::AdapterMissing => {
            "structured Chat is not supported for this provider"
        }
        ConversationReasonCode::Ready => "structured Chat is ready",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::api::schema::{ConversationPageDirection, PaneReportAgentSessionParams};
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    #[test]
    fn native_metadata_reaches_pane_tokens_and_session_clear_retracts_it() {
        let _guard = crate::integration::integration_env_lock();
        let base =
            std::env::temp_dir().join(format!("herdr-native-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("session.jsonl");
        std::fs::write(&path, concat!(
            "{\"type\":\"model_change\",\"id\":\"m\",\"parentId\":null,\"modelId\":\"native-model\"}\n",
            "{\"type\":\"message\",\"id\":\"a\",\"parentId\":\"m\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}],\"usage\":{\"input\":12,\"output\":0}}}\n",
        )).unwrap();
        let old = std::env::var_os("PI_CODING_AGENT_DIR");
        std::env::set_var("PI_CODING_AGENT_DIR", &base);
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("native metadata")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public = app.public_pane_id(0, pane_id).unwrap();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.integration_token = Some("test-token".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.metadata_tokens.patch(
            std::collections::HashMap::from([("git_branch".into(), Some("main".into()))]),
            None,
            std::time::Instant::now(),
        );
        app.handle_pane_report_agent_session(
            "session".into(),
            PaneReportAgentSessionParams {
                pane_id: public.clone(),
                source: "herdr:pi".into(),
                agent: "pi".into(),
                seq: Some(1),
                agent_session_id: None,
                agent_session_path: Some(path.to_string_lossy().into()),
                session_start_source: Some("startup".into()),
                integration_token: Some("test-token".into()),
            },
        );
        let params = || AgentConversationReadParams {
            target: public.clone(),
            cursor: None,
            direction: ConversationPageDirection::Newest,
            limit: 10,
        };
        let response = app.handle_agent_conversation_read("read".into(), params());
        let result: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(result.get("error").is_none(), "{response}");
        let info = app.pane_info(0, pane_id).unwrap();
        assert_eq!(info.tokens["model"], "native-model");
        assert_eq!(info.tokens["input_tokens"], "12");
        assert_eq!(info.tokens["output_tokens"], "0");
        assert_eq!(info.tokens["usage_scope"], "last_response");
        let revision = info.revision;
        app.handle_agent_conversation_read("unchanged".into(), params());
        assert_eq!(app.pane_info(0, pane_id).unwrap().revision, revision);
        // A later explicit report owns its value even when the native source
        // is subsequently retired. Only still-native values are retracted.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .metadata_tokens
            .patch(
                std::collections::HashMap::from([("model".into(), Some("user-override".into()))]),
                None,
                std::time::Instant::now(),
            );
        app.state.conversation_sources.clear(pane_id);
        app.sync_conversation_cache_for_pane(pane_id);
        let info = app.pane_info(0, pane_id).unwrap();
        assert_eq!(info.tokens["model"], "user-override");
        assert!(!info.tokens.contains_key("input_tokens"));
        assert_eq!(info.tokens["git_branch"], "main");
        if let Some(old) = old {
            std::env::set_var("PI_CODING_AGENT_DIR", old);
        } else {
            std::env::remove_var("PI_CODING_AGENT_DIR");
        }
        let _ = std::fs::remove_dir_all(base);
    }
}
