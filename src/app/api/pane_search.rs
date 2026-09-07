use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

use crate::api::schema::{PaneSearchDirection, PaneSearchParams, PaneSearchResult, ResponseResult};
use crate::app::App;
use crate::pane::{TerminalTextMatch, TerminalTextPoint};

use super::responses::{encode_error, encode_success};

const MAX_QUERY_BYTES: usize = 4096;
const MAX_CURSOR_BYTES: usize = 16384;

// Internal cursor format: clients must treat this as opaque. Bind the anchor to
// its terminal and query so it cannot accidentally navigate another search.
#[derive(Serialize, Deserialize)]
struct SearchCursor {
    version: u8,
    terminal_id: String,
    query: String,
    case_sensitive: bool,
    start: (u32, u16),
    end: (u32, u16),
    fingerprint: u64,
    cols: u16,
    alternate: bool,
}

impl SearchCursor {
    fn decode(value: &str, params: &PaneSearchParams) -> Option<TerminalTextMatch> {
        if value.len() > MAX_CURSOR_BYTES {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
        let cursor: Self = serde_json::from_slice(&bytes).ok()?;
        if cursor.version != 1
            || cursor.terminal_id != params.terminal_id
            || cursor.query != URL_SAFE_NO_PAD.encode(params.query.as_bytes())
            || cursor.case_sensitive != params.case_sensitive
        {
            return None;
        }
        let start = TerminalTextPoint {
            row: cursor.start.0,
            col: cursor.start.1,
        };
        let end = TerminalTextPoint {
            row: cursor.end.0,
            col: cursor.end.1,
        };
        if start > end || start.col >= cursor.cols || end.col >= cursor.cols {
            return None;
        }
        Some(TerminalTextMatch {
            start,
            end,
            source_fingerprint: cursor.fingerprint,
            scan_cols: cursor.cols,
            scan_screen: if cursor.alternate {
                crate::ghostty::ActiveScreen::Alternate
            } else {
                crate::ghostty::ActiveScreen::Primary
            },
        })
    }

    fn encode(params: &PaneSearchParams, selected: TerminalTextMatch) -> Option<String> {
        let cursor = Self {
            version: 1,
            terminal_id: params.terminal_id.clone(),
            query: URL_SAFE_NO_PAD.encode(params.query.as_bytes()),
            case_sensitive: params.case_sensitive,
            start: (selected.start.row, selected.start.col),
            end: (selected.end.row, selected.end.col),
            fingerprint: selected.source_fingerprint,
            cols: selected.scan_cols,
            alternate: selected.scan_screen == crate::ghostty::ActiveScreen::Alternate,
        };
        serde_json::to_vec(&cursor)
            .ok()
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
    }
}

fn selected_index(
    matches: &[TerminalTextMatch],
    direction: PaneSearchDirection,
    previous: Option<TerminalTextMatch>,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    match direction {
        PaneSearchDirection::First => Some(0),
        PaneSearchDirection::Next => previous
            .and_then(|previous| matches.iter().position(|item| item.start > previous.end))
            .or(Some(0)),
        PaneSearchDirection::Previous => previous
            .and_then(|previous| matches.iter().rposition(|item| item.end < previous.start))
            .or_else(|| matches.len().checked_sub(1)),
    }
}

impl App {
    pub(super) fn handle_pane_search(&mut self, id: String, params: PaneSearchParams) -> String {
        if params.query.len() > MAX_QUERY_BYTES {
            return encode_error(id, "invalid_params", "query exceeds 4096 UTF-8 bytes");
        }
        if params
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
        {
            return encode_error(id, "invalid_params", "cursor exceeds 16384 bytes");
        }
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some((_, pane)) = self.find_pane(pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        if pane.attached_terminal_id.to_string() != params.terminal_id {
            return encode_error(
                id,
                "terminal_mismatch",
                "pane is attached to a different terminal",
            );
        }
        let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some((runtime, _)) = self.lookup_runtime(ws_idx, pane_id) else {
            return encode_error(id, "pane_not_found", "pane terminal unavailable");
        };
        let matches = if params.query.is_empty() {
            Vec::new()
        } else {
            runtime.search_text_matches(&params.query, params.case_sensitive)
        };
        // Find the anchor in the fresh scan before validating it. In addition to
        // rejecting overwritten text this bounds work for untrusted cursor rows.
        let previous = params
            .cursor
            .as_deref()
            .and_then(|cursor| SearchCursor::decode(cursor, &params))
            .filter(|previous| matches.contains(previous))
            .filter(|previous| runtime.text_match_is_current(*previous));
        let index = selected_index(&matches, params.direction, previous);
        let mut cursor = None;
        let mut preview = None;
        if let Some(selected) = index.and_then(|index| matches.get(index).copied()) {
            let Some(context) = runtime.reveal_text_match(selected) else {
                return encode_error(
                    id,
                    "search_changed",
                    "terminal output changed; retry search",
                );
            };
            cursor = SearchCursor::encode(&params, selected);
            if cursor.is_none() {
                return encode_error(id, "search_unavailable", "could not encode search cursor");
            }
            preview = Some(context);
        }
        encode_success(
            id,
            ResponseResult::PaneSearch {
                search: PaneSearchResult {
                    pane_id: public_pane_id,
                    terminal_id: params.terminal_id,
                    query: params.query,
                    case_sensitive: params.case_sensitive,
                    match_count: matches.len(),
                    match_index: index,
                    cursor,
                    preview,
                },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};
    use crate::{config::Config, layout::PaneId, terminal::TerminalRuntime, workspace::Workspace};

    fn fixture(cols: u16, rows: u16, text: &str) -> (App, PaneSearchParams, PaneId) {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("search")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .to_string();
        app.state.insert_test_runtime(
            pane_id,
            TerminalRuntime::test_with_scrollback_bytes(cols, rows, 10_000_000, text.as_bytes()),
        );
        let params = PaneSearchParams {
            pane_id: app.public_pane_id(0, pane_id).unwrap(),
            terminal_id,
            query: "needle".into(),
            case_sensitive: false,
            direction: PaneSearchDirection::First,
            cursor: None,
        };
        (app, params, pane_id)
    }

    fn search(app: &mut App, params: PaneSearchParams) -> PaneSearchResult {
        let response = app.handle_api_request(Request {
            id: "search-test".into(),
            method: Method::PaneSearch(params),
        });
        let success: SuccessResponse = serde_json::from_str(&response).expect(&response);
        let ResponseResult::PaneSearch { search } = success.result else {
            panic!("unexpected search response")
        };
        search
    }

    #[tokio::test]
    async fn pane_search_finds_history_beyond_1000_rows_and_centers_match() {
        let mut text = "old needle context\r\n".to_string();
        text.push_str(&"filler\r\n".repeat(1300));
        text.push_str("new needle context\r\n");
        let (mut app, mut params, pane_id) = fixture(40, 7, &text);
        let first = search(&mut app, params.clone());
        assert_eq!(first.match_count, 2);
        assert_eq!(first.match_index, Some(0));
        assert!(first
            .preview
            .as_deref()
            .unwrap()
            .contains("old needle context"));
        let runtime = app
            .state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap();
        let metrics = runtime.scroll_metrics().unwrap();
        assert!(metrics.offset_from_bottom > 1000);
        assert!(runtime.visible_text().contains("old needle context"));
        params.cursor = first.cursor;
        params.direction = PaneSearchDirection::Next;
        let next = search(&mut app, params.clone());
        assert_eq!(next.match_index, Some(1));
        assert!(next
            .preview
            .as_deref()
            .unwrap()
            .contains("new needle context"));
        params.cursor = next.cursor;
        let wrapped = search(&mut app, params.clone());
        assert_eq!(wrapped.match_index, Some(0));
        params.cursor = wrapped.cursor;
        params.direction = PaneSearchDirection::Previous;
        assert_eq!(search(&mut app, params).match_index, Some(1));
    }

    #[tokio::test]
    async fn pane_search_empty_and_missing_queries_preserve_viewport() {
        let (mut app, mut params, pane_id) =
            fixture(20, 3, &format!("needle\r\n{}", "filler\r\n".repeat(30)));
        search(&mut app, params.clone());
        let before = app
            .state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap()
            .scroll_metrics();
        for query in ["", "absent"] {
            params.query = query.into();
            let result = search(&mut app, params.clone());
            assert_eq!(result.match_count, 0);
            assert_eq!(result.match_index, None);
            assert_eq!(result.cursor, None);
            assert_eq!(result.preview, None);
            assert_eq!(
                app.state
                    .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
                    .unwrap()
                    .scroll_metrics(),
                before
            );
        }
    }

    #[tokio::test]
    async fn pane_search_unicode_soft_wrap_and_literal_case_matching() {
        let (mut app, mut params, _) = fixture(5, 4, "ab界e\u{301}XYZ\r\nAB界E\u{301}xyz\r\n");
        params.query = "界e\u{301}XYZ".into();
        assert_eq!(search(&mut app, params.clone()).match_count, 2);
        params.case_sensitive = true;
        let result = search(&mut app, params);
        assert_eq!(result.match_count, 1);
        assert!(result.preview.unwrap().contains('界'));
    }

    #[tokio::test]
    async fn pane_search_rejects_terminal_mismatch_and_oversized_input() {
        let (mut app, params, _) = fixture(20, 3, "needle");
        for (bad, code) in [
            (
                PaneSearchParams {
                    terminal_id: "other".into(),
                    ..params.clone()
                },
                "terminal_mismatch",
            ),
            (
                PaneSearchParams {
                    query: "é".repeat(2049),
                    ..params.clone()
                },
                "invalid_params",
            ),
            (
                PaneSearchParams {
                    cursor: Some("x".repeat(16385)),
                    ..params.clone()
                },
                "invalid_params",
            ),
        ] {
            let response = app.handle_pane_search("bad".into(), bad);
            let error: ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, code);
        }
    }

    #[tokio::test]
    async fn pane_search_ignores_invalid_stale_and_changed_query_cursors() {
        let (mut app, mut params, pane_id) = fixture(30, 4, "needle first\r\nneedle second");
        let first = search(&mut app, params.clone());
        params.direction = PaneSearchDirection::Next;
        params.cursor = Some("bad cursor".into());
        assert_eq!(search(&mut app, params.clone()).match_index, Some(0));
        params.cursor = first.cursor;
        // Replace output under the same terminal identity. The old coordinates
        // now contain different text, so next must restart instead of skipping.
        app.state.insert_test_runtime(
            pane_id,
            TerminalRuntime::test_with_scrollback_bytes(
                30,
                4,
                10_000_000,
                b"changed\r\nneedle second",
            ),
        );
        let changed = search(&mut app, params.clone());
        assert_eq!(changed.match_count, 1);
        assert_eq!(changed.match_index, Some(0));
        params.query = "second".into();
        assert_eq!(search(&mut app, params).match_count, 1);
    }

    #[tokio::test]
    async fn pane_search_preserves_cursor_on_append_and_resets_after_reflow() {
        let (mut app, mut params, pane_id) = fixture(30, 4, "needle first\r\nneedle second");
        let first = search(&mut app, params.clone());
        params.direction = PaneSearchDirection::Next;
        params.cursor = first.cursor;
        app.state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\r\nneedle third");
        let appended = search(&mut app, params.clone());
        assert_eq!(appended.match_count, 3);
        assert_eq!(appended.match_index, Some(1));
        params.cursor = appended.cursor;
        let runtime = app
            .state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap();
        runtime.resize(4, 10, 0, 0);
        let resized = search(&mut app, params);
        assert_eq!(resized.match_index, Some(0));
        assert!(resized.preview.unwrap().contains("needle"));
    }

    #[tokio::test]
    async fn pane_search_cleared_history_only_navigates_current_matches() {
        let (mut app, mut params, pane_id) = fixture(20, 3, "needle old\r\nneedle last");
        let first = search(&mut app, params.clone());
        params.cursor = first.cursor;
        params.direction = PaneSearchDirection::Next;
        let runtime = app
            .state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap();
        runtime.test_process_pty_bytes(b"\x1b[3J\x1b[2J\x1b[Hgone\r\nneedle retained");
        let result = search(&mut app, params);
        assert_eq!(result.match_count, 1);
        assert_eq!(result.match_index, Some(0));
        assert!(result.preview.unwrap().contains("needle retained"));
    }

    #[tokio::test]
    async fn pane_search_eviction_of_repeated_text_keeps_navigation_in_retained_matches() {
        let (mut app, mut params, pane_id) = fixture(20, 3, "");
        app.state.insert_test_runtime(
            pane_id,
            TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                "needle repeated\r\n".repeat(300).as_bytes(),
            ),
        );
        let first = search(&mut app, params.clone());
        assert!(first.match_count > 0);
        params.cursor = first.cursor;
        params.direction = PaneSearchDirection::Next;
        app.state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap()
            .test_process_pty_bytes("needle repeated\r\n".repeat(30000).as_bytes());
        let result = search(&mut app, params);
        assert!(
            result.match_count < 30300,
            "history must actually be evicted"
        );
        assert!(result.match_index.unwrap() < result.match_count);
        assert!(result.preview.unwrap().contains("needle repeated"));
        let runtime = app
            .state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
            .unwrap();
        assert!(runtime.visible_text().contains("needle repeated"));
    }

    #[test]
    fn pane_search_is_a_ui_mutation_and_cursor_stays_bounded() {
        let params = PaneSearchParams {
            pane_id: "w1:p1".into(),
            terminal_id: "t1".into(),
            query: "\0".repeat(4096),
            case_sensitive: false,
            direction: PaneSearchDirection::First,
            cursor: None,
        };
        let selected = TerminalTextMatch {
            start: TerminalTextPoint { row: 0, col: 0 },
            end: TerminalTextPoint { row: 0, col: 0 },
            source_fingerprint: 1,
            scan_cols: 80,
            scan_screen: crate::ghostty::ActiveScreen::Primary,
        };
        let cursor = SearchCursor::encode(&params, selected).unwrap();
        assert!(cursor.len() < MAX_CURSOR_BYTES);
        assert_eq!(SearchCursor::decode(&cursor, &params), Some(selected));
        assert!(crate::api::request_changes_ui(&Request {
            id: "test".into(),
            method: Method::PaneSearch(params)
        }));
    }
}
