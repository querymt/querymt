//! `history_page`: merge an older-history response in front of the view.

use crate::core::SessionCore;
use crate::snapshot::{decode_events, optional};
use crate::store::Store;
use crate::view::{ChangeReport, HistoryPaging, InputError};
use querymt_agent_protocol_types::session::history::{HistoryInfo, TruncatedEntry};
use querymt_agent_protocol_types::session::load_snapshot::UserPromptRecord;
use serde_json::Value;

impl SessionCore {
    /// Apply the raw `_querymt/session/history` response. Events already in
    /// the view are skipped; live state (streaming turn, optimistic prompts)
    /// is untouched because the page is built apart and prepended.
    pub(crate) fn history_page(&mut self, response: &Value, index: u32, report: &mut ChangeReport) {
        let parsed = (|| -> Result<_, String> {
            let raw = response
                .get("events")
                .and_then(Value::as_array)
                .ok_or_else(|| "history page `events` is not an array".to_owned())?;
            let info: HistoryInfo = crate::snapshot::required(response, "history")?;
            let truncated: Vec<TruncatedEntry> =
                optional(response, "truncated")?.unwrap_or_default();
            let prompts: Vec<UserPromptRecord> =
                optional(response, "userPrompts")?.unwrap_or_default();
            Ok((raw, info, truncated, prompts))
        })();
        let (raw, info, truncated, prompts) = match parsed {
            Ok(parsed) => parsed,
            Err(message) => {
                report.errors.push(InputError {
                    input_index: index,
                    seq: None,
                    message,
                });
                return;
            }
        };
        let (mut events, unknown, errors) = decode_events(raw, index);
        report.unknown += unknown;
        report.errors.extend(errors);
        events.retain(|event| !self.seqs.contains(&event.seq));
        events.sort_by_key(|event| event.seq);

        // Build the page apart; restore the run state the replay would touch.
        let run_state = self.header.run_state;
        let active_tool = self.header.active_tool_call_id.clone();
        let fresh = Store::continuing(self.store.revision(), self.store.anon_counter());
        let live = std::mem::replace(&mut self.store, fresh);
        self.replay_events(&events, &prompts);
        let page = std::mem::replace(&mut self.store, live);
        self.header.run_state = run_state;
        self.header.active_tool_call_id = active_tool;

        self.store.prepend(page);
        for entry in &truncated {
            self.mark_truncated(entry);
        }
        self.header.history = HistoryPaging {
            has_older: !info.complete,
            before: info.before,
            stale: info.stale.unwrap_or(false),
        };
    }
}
