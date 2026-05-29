use crate::{EditPredictionStore, zeta::zeta2_prompt_input};
use anyhow::{Context as _, Result};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    interpolate_edits,
};
use futures::{AsyncReadExt as _, FutureExt as _, select_biased};
use gpui::{
    App, AsyncApp, Context, Entity, Task, TaskExt as _,
    http_client::{self, AsyncBody, HttpClient},
};
use icons::IconName;
use language::{Anchor, Buffer, BufferSnapshot, EditPreview, Point, ToOffset as _, ToPoint as _};
use project::{CodeAction, Completion, LspAction, Project};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use text::Bias;

const LSP_SUGGESTED_ITEMS_TIMEOUT: Duration = Duration::from_millis(120);
const MAX_LSP_SUGGESTED_ITEMS: usize = 40;

#[derive(Clone)]
enum CurrentExternalPrediction {
    Local {
        id: Option<Arc<str>>,
        buffer: Entity<Buffer>,
        snapshot: BufferSnapshot,
        edits: Arc<[(Range<Anchor>, Arc<str>)]>,
        edit_preview: EditPreview,
    },
    Jump {
        id: Option<Arc<str>>,
        snapshot: BufferSnapshot,
        target: Anchor,
        should_retrigger: bool,
    },
}

pub struct ExternalEditPredictionDelegate {
    project: Entity<Project>,
    edit_prediction_store: Entity<EditPredictionStore>,
    http_client: Arc<dyn HttpClient>,
    pending_request: Option<Task<Result<()>>>,
    current_prediction: Option<CurrentExternalPrediction>,
    next_request_id: u64,
}

impl ExternalEditPredictionDelegate {
    pub fn new(
        project: Entity<Project>,
        edit_prediction_store: Entity<EditPredictionStore>,
        http_client: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            project,
            edit_prediction_store,
            http_client,
            pending_request: None,
            current_prediction: None,
            next_request_id: 0,
        }
    }
}

impl EditPredictionDelegate for ExternalEditPredictionDelegate {
    fn name() -> &'static str {
        "external"
    }

    fn display_name() -> &'static str {
        "External Edit Prediction"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn show_tab_accept_marker() -> bool {
        true
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::AiEdit)
    }

    fn is_enabled(
        &self,
        _buffer: &Entity<Buffer>,
        _cursor_position: language::Anchor,
        _cx: &App,
    ) -> bool {
        true
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some() && self.current_prediction.is_none()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce: bool,
        cx: &mut Context<Self>,
    ) {
        let snapshot = buffer.read(cx).snapshot();
        if let Some(CurrentExternalPrediction::Local {
            snapshot: old_snapshot,
            edits,
            ..
        }) = &self.current_prediction
            && interpolate_edits(old_snapshot, &snapshot, edits).is_some()
        {
            return;
        }

        let api_url = language::language_settings::all_language_settings(None, cx)
            .edit_predictions
            .external
            .api_url
            .to_string();
        let http_client = self.http_client.clone();
        let project = self.project.clone();
        let edit_prediction_store = self.edit_prediction_store.clone();
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request_id = self.next_request_id;

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
            }

            let lsp_suggested_labels =
                lsp_suggested_labels(project.clone(), buffer.clone(), cursor_position, cx).await;
            let request = cx.update(|cx| {
                build_request(
                    &project,
                    &edit_prediction_store,
                    &snapshot,
                    cursor_position,
                    &lsp_suggested_labels,
                    cx,
                )
            })?;
            let current_path = request.path.clone();
            let request_body = serde_json::to_string(&request)?;
            let http_request = http_client::Request::builder()
                .method(http_client::Method::POST)
                .uri(api_url)
                .header("Content-Type", "application/json")
                .body(AsyncBody::from(request_body))?;

            let mut response = http_client
                .send(http_request)
                .await
                .context("failed to send external edit prediction request")?;
            let status = response.status();

            if !status.is_success() {
                let mut body = String::new();
                response.body_mut().read_to_string(&mut body).await?;
                anyhow::bail!("external edit prediction server error: {status} - {body}");
            }

            let mut body = String::new();
            response.body_mut().read_to_string(&mut body).await?;
            if body.trim().is_empty() {
                this.update(cx, |this, cx| {
                    if this.next_request_id != request_id {
                        return;
                    }
                    this.current_prediction = None;
                    this.pending_request = None;
                    cx.notify();
                })?;
                return Ok(());
            }

            let response: ExternalEditPredictionResponse =
                serde_json::from_str(&body).context("failed to parse external edit prediction")?;

            let prediction =
                prediction_from_response(&project, &buffer, &snapshot, current_path, response, cx)
                    .await?;

            this.update(cx, |this, cx| {
                if this.next_request_id != request_id {
                    return;
                }
                this.current_prediction = prediction;
                this.pending_request = None;
                cx.notify();
            })?;

            Ok(())
        }));
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        if let Some(prediction) = self.current_prediction.take() {
            let prediction_id = match &prediction {
                CurrentExternalPrediction::Local { id, .. } => id.clone(),
                CurrentExternalPrediction::Jump { id, .. } => id.clone(),
            };
            if let Some(prediction_id) = prediction_id {
                let api_url = language::language_settings::all_language_settings(None, cx)
                    .edit_predictions
                    .external
                    .api_url
                    .to_string();
                let http_client = self.http_client.clone();
                cx.spawn(async move |_, _cx| {
                    send_accept_request(http_client, api_url, prediction_id.to_string()).await
                })
                .detach_and_log_err(cx);
            }

            if let CurrentExternalPrediction::Local { buffer, edits, .. } = prediction {
                let project = self.project.clone();
                cx.spawn(async move |_, cx| {
                    apply_import_quick_fix_after_accept(project, buffer, edits, cx).await
                })
                .detach_and_log_err(cx);
            }
        }
        self.pending_request = None;
    }

    fn partial_accept(&mut self, cx: &mut Context<Self>) {
        let prediction_id =
            self.current_prediction
                .as_ref()
                .and_then(|prediction| match prediction {
                    CurrentExternalPrediction::Local { id, .. }
                    | CurrentExternalPrediction::Jump { id, .. } => id.clone(),
                });

        if let Some(prediction_id) = prediction_id {
            let api_url = language::language_settings::all_language_settings(None, cx)
                .edit_predictions
                .external
                .api_url
                .to_string();
            let http_client = self.http_client.clone();
            cx.spawn(async move |_, _cx| {
                send_partial_accept_request(http_client, api_url, prediction_id.to_string()).await
            })
            .detach_and_log_err(cx);
        }
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, cx: &mut Context<Self>) {
        let prediction_id =
            self.current_prediction
                .as_ref()
                .and_then(|prediction| match prediction {
                    CurrentExternalPrediction::Local { id, .. }
                    | CurrentExternalPrediction::Jump { id, .. } => id.clone(),
                });

        if let Some(prediction_id) = prediction_id {
            let api_url = language::language_settings::all_language_settings(None, cx)
                .edit_predictions
                .external
                .api_url
                .to_string();
            let http_client = self.http_client.clone();
            cx.spawn(async move |_, _cx| {
                send_reject_request(http_client, api_url, prediction_id.to_string()).await
            })
            .detach_and_log_err(cx);
        }

        self.current_prediction = None;
        self.pending_request = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let buffer_snapshot = buffer.read(cx).snapshot();
        edit_prediction_from_current_prediction(self.current_prediction.as_ref()?, &buffer_snapshot)
    }
}

fn edit_prediction_from_current_prediction(
    prediction: &CurrentExternalPrediction,
    buffer_snapshot: &BufferSnapshot,
) -> Option<EditPrediction> {
    match prediction {
        CurrentExternalPrediction::Local {
            id,
            buffer: _,
            snapshot,
            edits,
            edit_preview,
        } => {
            let edits = interpolate_edits(snapshot, buffer_snapshot, edits)?;
            if edits.is_empty() {
                return None;
            }
            Some(EditPrediction::Local {
                id: id.as_ref().map(|id| id.to_string().into()),
                edits,
                cursor_position: None,
                edit_preview: Some(edit_preview.clone()),
            })
        }
        CurrentExternalPrediction::Jump {
            id,
            snapshot,
            target,
            should_retrigger,
        } => Some(EditPrediction::Jump {
            id: id.as_ref().map(|id| id.to_string().into()),
            snapshot: snapshot.clone(),
            target: *target,
            should_retrigger: *should_retrigger,
        }),
    }
}

fn build_request(
    project: &Entity<Project>,
    edit_prediction_store: &Entity<EditPredictionStore>,
    snapshot: &BufferSnapshot,
    cursor_position: Anchor,
    lsp_suggested_labels: &[String],
    cx: &mut App,
) -> Result<ExternalEditPredictionRequest> {
    let file = snapshot.file();
    let path = file
        .as_ref()
        .map(|file| file.full_path(cx).to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".to_string());
    let absolute_path = file
        .as_ref()
        .and_then(|file| file.as_local().map(|file| file.abs_path(cx)))
        .map(|path| path.to_string_lossy().into_owned());
    let workspace_root = file.as_ref().and_then(|file| {
        let project_path = project::ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        };
        project
            .read(cx)
            .get_workspace_root(&project_path, cx)
            .map(|path| path.to_string_lossy().into_owned())
    });
    let language = snapshot
        .language()
        .map(|language| language.name().to_string());
    let cursor = cursor_position.to_point(snapshot);
    let contents = snapshot
        .text_for_range(Point::new(0, 0)..snapshot.max_point())
        .collect::<String>();
    let cursor_request = build_cursor_request(
        project,
        edit_prediction_store,
        snapshot,
        &path,
        absolute_path.as_deref(),
        workspace_root.as_deref(),
        language.as_deref(),
        &contents,
        cursor,
        cursor_position,
        lsp_suggested_labels,
        cx,
    );

    Ok(ExternalEditPredictionRequest {
        version: 1,
        path,
        absolute_path,
        workspace_root,
        language,
        contents,
        cursor: cursor.into(),
        cursor_request,
    })
}

fn build_cursor_request(
    project: &Entity<Project>,
    edit_prediction_store: &Entity<EditPredictionStore>,
    snapshot: &BufferSnapshot,
    path: &str,
    absolute_path: Option<&str>,
    workspace_root: Option<&str>,
    language: Option<&str>,
    contents: &str,
    cursor: Point,
    cursor_position: Anchor,
    lsp_suggested_labels: &[String],
    cx: &mut App,
) -> Option<Value> {
    let relative_path = relative_cursor_path(path, absolute_path, workspace_root);
    let language_id = language_id(language);
    let line_ending = if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let line_count = contents.split(line_ending).count();
    let cursor_offset = cursor_position.to_offset(snapshot);

    let (events, related_files) = edit_prediction_store.update(cx, |store, cx| {
        (
            store
                .edit_history_for_project(project, cx)
                .into_iter()
                .map(|event| event.event)
                .collect::<Vec<_>>(),
            store.context_for_project(project, cx),
        )
    });

    let excerpt_path: Arc<Path> = PathBuf::from(&relative_path).into();
    let diagnostic_search_range =
        Point::new(cursor.row.saturating_sub(20), 0)..Point::new(cursor.row + 20, 0);
    let (_, prompt_input) = zeta2_prompt_input(
        snapshot,
        related_files.clone(),
        events.clone(),
        diagnostic_search_range,
        excerpt_path,
        cursor_offset,
        false,
        false,
        None,
    );

    let file_diff_histories = cursor_file_diff_histories(&events);
    let diff_history = file_diff_histories
        .iter()
        .find_map(|history| {
            (history["fileName"].as_str()? == relative_path).then(|| history["diffHistory"].clone())
        })
        .unwrap_or_else(|| json!([]));
    let mut additional_files = cursor_additional_files(&related_files);
    append_open_buffer_additional_files(
        &mut additional_files,
        project,
        &relative_path,
        workspace_root,
        cx,
    );
    let code_results = cursor_code_results(&related_files);
    let linter_errors = cursor_linter_errors(
        &relative_path,
        contents,
        &prompt_input.active_buffer_diagnostics,
    );
    let diagnostics = cursor_diagnostics(&prompt_input.active_buffer_diagnostics);
    let lsp_suggested_items = cursor_lsp_suggested_items(lsp_suggested_labels);

    Some(json!({
        "currentFile": {
            "relativeWorkspacePath": relative_path,
            "contents": contents,
            "cursorPosition": {
                "line": cursor.row,
                "column": cursor.column,
            },
            "dataframes": [],
            "languageId": language_id,
            "diagnostics": diagnostics,
            "totalNumberOfLines": line_count,
            "contentsStartAtLine": 0,
            "topChunks": [],
            "fileVersion": 0,
            "cellStartLines": [],
            "cells": [],
            "relyOnFilesync": false,
            "workspaceRootPath": workspace_root.unwrap_or_default(),
            "lineEnding": line_ending,
        },
        "diffHistory": diff_history,
        "diffHistoryKeys": [],
        "fileDiffHistories": file_diff_histories,
        "mergedDiffHistories": [],
        "blockDiffPatches": [],
        "contextItems": [],
        "parameterHints": [],
        "lspContexts": [],
        "cppIntentInfo": { "source": "line_change" },
        "enableMoreContext": true,
        "workspaceId": workspace_root.unwrap_or_default(),
        "additionalFiles": additional_files,
        "clientTime": 0,
        "filesyncUpdates": [],
        "timeSinceRequestStart": 0,
        "timeAtRequestSend": 0,
        "clientTimezoneOffset": 0,
        "lspSuggestedItems": lsp_suggested_items,
        "supportsCpt": false,
        "supportsCrlfCpt": false,
        "codeResults": code_results,
        "linterErrors": linter_errors,
    }))
}

fn relative_cursor_path(
    path: &str,
    absolute_path: Option<&str>,
    workspace_root: Option<&str>,
) -> String {
    if let (Some(absolute_path), Some(workspace_root)) = (absolute_path, workspace_root)
        && let Ok(relative_path) = Path::new(absolute_path).strip_prefix(workspace_root)
    {
        return relative_path.to_string_lossy().into_owned();
    }

    path.to_string()
}

fn language_id(language: Option<&str>) -> String {
    language
        .unwrap_or("plaintext")
        .to_ascii_lowercase()
        .split_whitespace()
        .collect()
}

fn cursor_file_diff_histories(events: &[Arc<zeta_prompt::Event>]) -> Vec<Value> {
    let mut histories_by_path = HashMap::<String, Vec<String>>::new();

    for event in events.iter().rev().take(20).rev() {
        let zeta_prompt::Event::BufferChange { path, diff, .. } = event.as_ref();
        histories_by_path
            .entry(path.to_string_lossy().into_owned())
            .or_default()
            .push(diff.clone());
    }

    histories_by_path
        .into_iter()
        .map(|(file_name, diff_history)| {
            let timestamps = vec![0; diff_history.len()];
            json!({
                "fileName": file_name,
                "diffHistory": diff_history,
                "diffHistoryTimestamps": timestamps,
            })
        })
        .collect()
}

fn cursor_additional_files(related_files: &[zeta_prompt::RelatedFile]) -> Vec<Value> {
    related_files
        .iter()
        .map(|file| {
            let visible_range_content = file
                .excerpts
                .iter()
                .map(|excerpt| excerpt.text.to_string())
                .collect::<Vec<_>>();
            let start_line_number_one_indexed = file
                .excerpts
                .iter()
                .map(|excerpt| excerpt.row_range.start as i32 + 1)
                .collect::<Vec<_>>();
            let visible_ranges = file
                .excerpts
                .iter()
                .map(|excerpt| {
                    json!({
                        "startLineNumber": excerpt.row_range.start as i32 + 1,
                        "endLineNumberInclusive": excerpt.row_range.end as i32,
                    })
                })
                .collect::<Vec<_>>();

            json!({
                "relativeWorkspacePath": file.path.to_string_lossy(),
                "isOpen": true,
                "visibleRangeContent": visible_range_content,
                "startLineNumberOneIndexed": start_line_number_one_indexed,
                "visibleRanges": visible_ranges,
            })
        })
        .collect()
}

fn append_open_buffer_additional_files(
    additional_files: &mut Vec<Value>,
    project: &Entity<Project>,
    current_relative_path: &str,
    workspace_root: Option<&str>,
    cx: &mut App,
) {
    const MAX_OPEN_BUFFERS: usize = 8;
    const MAX_OPEN_BUFFER_LINES: u32 = 200;

    let mut seen_paths = additional_files
        .iter()
        .filter_map(|file| file["relativeWorkspacePath"].as_str().map(String::from))
        .collect::<HashSet<_>>();
    seen_paths.insert(current_relative_path.to_string());

    for buffer in project
        .read(cx)
        .opened_buffers(cx)
        .into_iter()
        .take(MAX_OPEN_BUFFERS)
    {
        let snapshot = buffer.read(cx).snapshot();
        let Some(file) = snapshot.file() else {
            continue;
        };
        let path = file.full_path(cx).to_string_lossy().into_owned();
        let absolute_path = file
            .as_local()
            .map(|file| file.abs_path(cx).to_string_lossy().into_owned());
        let relative_path = relative_cursor_path(&path, absolute_path.as_deref(), workspace_root);
        if !seen_paths.insert(relative_path.clone()) {
            continue;
        }

        let max_point = snapshot.max_point();
        let end_row = max_point.row.min(MAX_OPEN_BUFFER_LINES.saturating_sub(1));
        let end = Point::new(end_row, snapshot.line_len(end_row));
        let text = snapshot
            .text_for_range(Point::new(0, 0)..end)
            .collect::<String>();
        if text.trim().is_empty() {
            continue;
        }

        additional_files.push(json!({
            "relativeWorkspacePath": relative_path,
            "isOpen": true,
            "visibleRangeContent": [text],
            "startLineNumberOneIndexed": [1],
            "visibleRanges": [{
                "startLineNumber": 1,
                "endLineNumberInclusive": end_row + 1,
            }],
        }));
    }
}

fn cursor_code_results(related_files: &[zeta_prompt::RelatedFile]) -> Vec<Value> {
    related_files
        .iter()
        .flat_map(|file| {
            file.excerpts.iter().map(move |excerpt| {
                json!({
                    "codeBlock": {
                        "relativeWorkspacePath": file.path.to_string_lossy(),
                        "range": {
                            "startPosition": {
                                "line": excerpt.row_range.start,
                                "column": 0,
                            },
                            "endPosition": {
                                "line": excerpt.row_range.end,
                                "column": 0,
                            },
                        },
                        "contents": excerpt.text,
                    },
                    "score": 0.8,
                })
            })
        })
        .collect()
}

fn cursor_linter_errors(
    relative_path: &str,
    contents: &str,
    diagnostics: &[zeta_prompt::ActiveBufferDiagnostic],
) -> Value {
    let errors = diagnostics
        .iter()
        .map(|diagnostic| {
            let start = position_in_text(
                &diagnostic.snippet,
                diagnostic.diagnostic_range_in_snippet.start,
            );
            let end = position_in_text(
                &diagnostic.snippet,
                diagnostic.diagnostic_range_in_snippet.end,
            );
            json!({
                "message": diagnostic.message,
                "range": {
                    "startLine": diagnostic.snippet_buffer_row_range.start + start.row,
                    "startColumn": start.column,
                    "endLine": diagnostic.snippet_buffer_row_range.start + end.row,
                    "endColumn": end.column,
                },
                "relatedInformation": [],
                "severity": diagnostic.severity.unwrap_or_default(),
                "isStale": false,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "relativeWorkspacePath": relative_path,
        "errors": errors,
        "fileContents": contents,
    })
}

async fn lsp_suggested_labels(
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    cursor_position: Anchor,
    cx: &mut AsyncApp,
) -> Vec<String> {
    let completions_task = cx.update(|cx| {
        project.update(cx, |project, cx| {
            project.completions(
                &buffer,
                cursor_position,
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                },
                cx,
            )
        })
    });

    let timeout = cx
        .background_executor()
        .timer(LSP_SUGGESTED_ITEMS_TIMEOUT)
        .fuse();
    let completions = completions_task.fuse();
    futures::pin_mut!(completions, timeout);

    let responses = select_biased! {
        response = completions => response.unwrap_or_else(|err| {
            log::debug!("failed to fetch LSP suggestions for Cursor payload: {err:#}");
            Vec::new()
        }),
        () = timeout => Vec::new(),
    };

    let mut seen = HashSet::new();
    responses
        .into_iter()
        .flat_map(|response| response.completions)
        .filter_map(|completion| cursor_lsp_suggestion_label(&completion))
        .filter(|label| seen.insert(label.clone()))
        .take(MAX_LSP_SUGGESTED_ITEMS)
        .collect()
}

fn cursor_lsp_suggestion_label(completion: &Completion) -> Option<String> {
    completion
        .source
        .lsp_completion(false)
        .map(|completion| completion.label.trim().to_string())
        .or_else(|| {
            let label = completion.label.text.trim();
            (!label.is_empty()).then(|| label.to_string())
        })
        .filter(|label| !label.is_empty())
}

fn cursor_lsp_suggested_items(labels: &[String]) -> Value {
    json!({
        "suggestions": labels
            .iter()
            .filter_map(|label| {
                let label = label.trim();
                (!label.is_empty()).then(|| json!({ "label": label }))
            })
            .collect::<Vec<_>>()
    })
}

fn cursor_diagnostics(diagnostics: &[zeta_prompt::ActiveBufferDiagnostic]) -> Value {
    let diagnostics = diagnostics
        .iter()
        .map(|diagnostic| {
            json!({
                "message": diagnostic.message,
                "range": cursor_diagnostic_range(diagnostic),
                "severity": diagnostic.severity.unwrap_or_default(),
                "relatedInformation": [],
            })
        })
        .collect::<Vec<_>>();

    json!(diagnostics)
}

fn cursor_diagnostic_range(diagnostic: &zeta_prompt::ActiveBufferDiagnostic) -> Value {
    let start = position_in_text(
        &diagnostic.snippet,
        diagnostic.diagnostic_range_in_snippet.start,
    );
    let end = position_in_text(
        &diagnostic.snippet,
        diagnostic.diagnostic_range_in_snippet.end,
    );

    json!({
        "startLine": diagnostic.snippet_buffer_row_range.start + start.row,
        "startColumn": start.column,
        "endLine": diagnostic.snippet_buffer_row_range.start + end.row,
        "endColumn": end.column,
    })
}

fn position_in_text(text: &str, offset: usize) -> Point {
    let mut row = 0;
    let mut column = 0;
    for (byte_offset, ch) in text.char_indices() {
        if byte_offset >= offset {
            break;
        }
        if ch == '\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    Point::new(row, column)
}

async fn prediction_from_response(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    snapshot: &BufferSnapshot,
    current_path: String,
    response: ExternalEditPredictionResponse,
    cx: &mut AsyncApp,
) -> Result<Option<CurrentExternalPrediction>> {
    let mut local_edits = Vec::new();
    for edit in response.edits {
        let edit_path = edit.path.as_deref().unwrap_or(&current_path);
        if edit_path != current_path {
            let jump = ExternalJump {
                path: edit.path.unwrap_or_default(),
                position: edit.range.start,
                expected_content: None,
                should_retrigger: Some(true),
            };
            return jump_prediction(project, response.id.clone(), jump, cx).await;
        }

        let start = point_for_position(snapshot, edit.range.start);
        let end = point_for_position(snapshot, edit.range.end);
        local_edits.push((
            snapshot.anchor_before(start)..snapshot.anchor_after(end),
            edit.text.into(),
        ));
    }

    if !local_edits.is_empty() {
        let edits: Arc<[_]> = local_edits.into();
        let edit_preview = buffer
            .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
            .await;
        return Ok(Some(CurrentExternalPrediction::Local {
            id: response.id.map(Into::into),
            buffer: buffer.clone(),
            snapshot: snapshot.clone(),
            edits,
            edit_preview,
        }));
    }

    if let Some(jump) = response.jump {
        return jump_prediction(project, response.id, jump, cx).await;
    }

    Ok(None)
}

async fn apply_import_quick_fix_after_accept(
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for delay_ms in IMPORT_QUICK_FIX_RETRY_DELAYS_MS {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(delay_ms))
            .await;

        let actions = project
            .update(cx, |project, cx| {
                let snapshot = buffer.read(cx).snapshot();
                let range = accepted_edit_search_range(&snapshot, &edits);
                project.code_actions(
                    &buffer,
                    range,
                    Some(import_quick_fix_code_action_kinds()),
                    cx,
                )
            })
            .await?
            .unwrap_or_default();

        let Some(action) = select_import_quick_fix_attempt(actions) else {
            continue;
        };

        project
            .update(cx, |project, cx| {
                project.apply_code_action(buffer.clone(), action, false, cx)
            })
            .await?;
        return Ok(());
    }

    Ok(())
}

const IMPORT_QUICK_FIX_RETRY_DELAYS_MS: [u64; 4] = [100, 250, 500, 900];

fn import_quick_fix_code_action_kinds() -> Vec<lsp::CodeActionKind> {
    vec![lsp::CodeActionKind::QUICKFIX, lsp::CodeActionKind::SOURCE]
}

async fn send_accept_request(
    http_client: Arc<dyn HttpClient>,
    api_url: String,
    id: String,
) -> Result<()> {
    send_fate_request(http_client, api_url, "accept", id).await
}

async fn send_reject_request(
    http_client: Arc<dyn HttpClient>,
    api_url: String,
    id: String,
) -> Result<()> {
    send_fate_request(http_client, api_url, "reject", id).await
}

async fn send_partial_accept_request(
    http_client: Arc<dyn HttpClient>,
    api_url: String,
    id: String,
) -> Result<()> {
    send_fate_request(http_client, api_url, "partial_accept", id).await
}

async fn send_fate_request(
    http_client: Arc<dyn HttpClient>,
    api_url: String,
    endpoint: &str,
    id: String,
) -> Result<()> {
    let request_body = serde_json::to_string(&ExternalAcceptRequest { id })?;
    let http_request = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(external_fate_url(&api_url, endpoint))
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(request_body))?;

    let mut response = http_client
        .send(http_request)
        .await
        .context("failed to send external edit prediction accept request")?;
    let status = response.status();
    if !status.is_success() {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!("external edit prediction {endpoint} server error: {status} - {body}");
    }

    Ok(())
}

#[cfg(test)]
fn external_accept_url(api_url: &str) -> String {
    external_fate_url(api_url, "accept")
}

#[cfg(test)]
fn external_reject_url(api_url: &str) -> String {
    external_fate_url(api_url, "reject")
}

#[cfg(test)]
fn external_partial_accept_url(api_url: &str) -> String {
    external_fate_url(api_url, "partial_accept")
}

fn external_fate_url(api_url: &str, endpoint: &str) -> String {
    api_url
        .strip_suffix("/predict")
        .map(|base| format!("{base}/{endpoint}"))
        .unwrap_or_else(|| format!("{}/{endpoint}", api_url.trim_end_matches('/')))
}

fn accepted_edit_search_range(
    snapshot: &BufferSnapshot,
    edits: &[(Range<Anchor>, Arc<str>)],
) -> Range<Point> {
    let mut start_row = u32::MAX;
    let mut end_row = 0;

    for (range, text) in edits {
        let start = range.start.to_point(snapshot);
        let end = range.end.to_point(snapshot);
        start_row = start_row.min(start.row);
        end_row = end_row
            .max(start.row + text.chars().filter(|ch| *ch == '\n').count() as u32)
            .max(end.row);
    }

    if start_row == u32::MAX {
        start_row = 0;
    }

    let max_point = snapshot.max_point();
    let start = Point::new(start_row.saturating_sub(3), 0);
    let end = Point::new((end_row + 3).min(max_point.row), max_point.column);
    start..end
}

#[cfg(test)]
fn select_import_quick_fix(actions: Vec<CodeAction>) -> Option<CodeAction> {
    select_import_quick_fix_from_attempts([actions]).map(|(_, action)| action)
}

fn select_import_quick_fix_attempt(actions: Vec<CodeAction>) -> Option<CodeAction> {
    actions
        .into_iter()
        .enumerate()
        .filter_map(|(index, action)| {
            import_quick_fix_priority(&action).map(|priority| (priority, index, action))
        })
        .min_by_key(|(priority, index, _)| (*priority, *index))
        .map(|(_, _, action)| action)
}

#[cfg(test)]
fn select_import_quick_fix_from_attempts(
    attempts: impl IntoIterator<Item = Vec<CodeAction>>,
) -> Option<(usize, CodeAction)> {
    attempts
        .into_iter()
        .enumerate()
        .find_map(|(attempt, actions)| {
            select_import_quick_fix_attempt(actions).map(|action| (attempt, action))
        })
}

fn import_quick_fix_priority(action: &CodeAction) -> Option<u8> {
    let disabled =
        matches!(&action.lsp_action, LspAction::Action(action) if action.disabled.is_some());
    if !is_import_code_action(
        action.lsp_action.action_kind().as_ref(),
        action.lsp_action.title(),
        disabled,
    ) {
        return None;
    }

    let kind = action
        .lsp_action
        .action_kind()
        .map(|kind| kind.as_str().to_ascii_lowercase());
    let title = action.lsp_action.title().to_ascii_lowercase();

    if title.contains("add import from")
        || title.contains("import from")
        || (title.starts_with("import ") && title.contains(" from "))
    {
        Some(0)
    } else if kind
        .as_deref()
        .is_some_and(|kind| kind.contains("quickfix"))
    {
        Some(1)
    } else if kind
        .as_deref()
        .is_some_and(|kind| kind.contains("addmissingimports"))
    {
        Some(2)
    } else {
        Some(3)
    }
}

fn is_import_code_action(kind: Option<&lsp::CodeActionKind>, title: &str, disabled: bool) -> bool {
    if disabled {
        return false;
    }

    let is_supported_kind = kind.map_or(true, |kind| {
        code_action_kind_matches(&lsp::CodeActionKind::QUICKFIX, kind)
            || code_action_kind_matches(&lsp::CodeActionKind::SOURCE, kind)
    });
    if !is_supported_kind {
        return false;
    }

    let kind = kind.map(|kind| kind.as_str().to_ascii_lowercase());
    let title = title.to_ascii_lowercase();
    let looks_like_import = title.contains("import")
        || kind
            .as_deref()
            .is_some_and(|kind| kind.contains("addmissingimports"));

    looks_like_import
        && !title.contains("organize imports")
        && !title.contains("fix all")
        && !title.contains("remove")
        && !title.contains("unused")
}

fn code_action_kind_matches(requested: &lsp::CodeActionKind, actual: &lsp::CodeActionKind) -> bool {
    let requested = requested.as_str();
    let actual = actual.as_str();
    actual == requested
        || actual
            .strip_prefix(requested)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

async fn jump_prediction(
    project: &Entity<Project>,
    id: Option<String>,
    jump: ExternalJump,
    cx: &mut AsyncApp,
) -> Result<Option<CurrentExternalPrediction>> {
    let Some(project_path) =
        project.read_with(cx, |project, cx| project.find_project_path(&jump.path, cx))
    else {
        return Ok(None);
    };

    let target_buffer = project
        .update(cx, |project, cx| project.open_buffer(project_path, cx))
        .await?;
    let (snapshot, target) = target_buffer.read_with(cx, |buffer, _cx| {
        let snapshot = buffer.snapshot();
        let target = snapshot.anchor_before(point_for_jump(&snapshot, &jump));
        (snapshot, target)
    });

    Ok(Some(CurrentExternalPrediction::Jump {
        id: id.map(Into::into),
        snapshot,
        target,
        should_retrigger: jump.should_retrigger.unwrap_or(false),
    }))
}

fn point_for_position(snapshot: &BufferSnapshot, position: ExternalPosition) -> Point {
    snapshot.clip_point(Point::new(position.line, position.column), Bias::Left)
}

fn point_for_jump(snapshot: &BufferSnapshot, jump: &ExternalJump) -> Point {
    let fallback = point_for_position(snapshot, jump.position);
    let Some(expected_content) = jump
        .expected_content
        .as_ref()
        .map(|content| content.trim())
        .filter(|content| !content.is_empty())
    else {
        return fallback;
    };

    let contents = snapshot
        .text_for_range(Point::new(0, 0)..snapshot.max_point())
        .collect::<String>();
    let lines = contents.split('\n').collect::<Vec<_>>();
    if lines.is_empty() {
        return fallback;
    }

    let target_row = fallback.row.min(lines.len().saturating_sub(1) as u32);
    for distance in 0..=8 {
        for row in [
            target_row.checked_sub(distance),
            target_row.checked_add(distance),
        ]
        .into_iter()
        .flatten()
        {
            let Some(line) = lines.get(row as usize) else {
                continue;
            };
            if let Some(column) = line.find(expected_content) {
                return Point::new(row, column as u32);
            }
            if line.trim() == expected_content {
                return Point::new(
                    row,
                    line.len().saturating_sub(line.trim_start().len()) as u32,
                );
            }
        }
    }

    fallback
}

#[derive(Serialize)]
struct ExternalEditPredictionRequest {
    version: u32,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    absolute_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    contents: String,
    cursor: ExternalPosition,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor_request: Option<Value>,
}

#[derive(Deserialize)]
struct ExternalEditPredictionResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    edits: Vec<ExternalEdit>,
    #[serde(default)]
    jump: Option<ExternalJump>,
}

#[derive(Serialize)]
struct ExternalAcceptRequest {
    id: String,
}

#[derive(Deserialize)]
struct ExternalEdit {
    #[serde(default)]
    path: Option<String>,
    range: ExternalRange,
    text: Arc<str>,
}

#[derive(Deserialize)]
struct ExternalJump {
    path: String,
    position: ExternalPosition,
    #[serde(default)]
    expected_content: Option<String>,
    #[serde(default, rename = "should_retrigger")]
    should_retrigger: Option<bool>,
}

#[derive(Deserialize)]
struct ExternalRange {
    start: ExternalPosition,
    end: ExternalPosition,
}

#[derive(Copy, Clone, Deserialize, Serialize)]
struct ExternalPosition {
    line: u32,
    column: u32,
}

impl From<Point> for ExternalPosition {
    fn from(point: Point) -> Self {
        Self {
            line: point.row,
            column: point.column,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CurrentExternalPrediction, EditPrediction, accepted_edit_search_range, cursor_diagnostics,
        cursor_linter_errors, cursor_lsp_suggested_items, edit_prediction_from_current_prediction,
        external_accept_url, external_partial_accept_url, external_reject_url,
        import_quick_fix_code_action_kinds, is_import_code_action, select_import_quick_fix,
        select_import_quick_fix_from_attempts,
    };
    use db::AppDatabase;
    use gpui::{AppContext as _, TestAppContext};
    use language::Buffer;
    use lsp::LanguageServerId;
    use project::{CodeAction, LspAction};
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(AppDatabase::test_new());
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[test]
    fn test_external_accept_url() {
        assert_eq!(
            external_accept_url("http://127.0.0.1:17878/predict"),
            "http://127.0.0.1:17878/accept"
        );
        assert_eq!(
            external_accept_url("http://127.0.0.1:17878/custom"),
            "http://127.0.0.1:17878/custom/accept"
        );
        assert_eq!(
            external_accept_url("http://127.0.0.1:17878/custom/"),
            "http://127.0.0.1:17878/custom/accept"
        );
    }

    #[test]
    fn test_external_reject_url() {
        assert_eq!(
            external_reject_url("http://127.0.0.1:17878/predict"),
            "http://127.0.0.1:17878/reject"
        );
        assert_eq!(
            external_reject_url("http://127.0.0.1:17878/custom"),
            "http://127.0.0.1:17878/custom/reject"
        );
        assert_eq!(
            external_reject_url("http://127.0.0.1:17878/custom/"),
            "http://127.0.0.1:17878/custom/reject"
        );
    }

    #[test]
    fn test_external_partial_accept_url() {
        assert_eq!(
            external_partial_accept_url("http://127.0.0.1:17878/predict"),
            "http://127.0.0.1:17878/partial_accept"
        );
        assert_eq!(
            external_partial_accept_url("http://127.0.0.1:17878/custom"),
            "http://127.0.0.1:17878/custom/partial_accept"
        );
        assert_eq!(
            external_partial_accept_url("http://127.0.0.1:17878/custom/"),
            "http://127.0.0.1:17878/custom/partial_accept"
        );
    }

    #[test]
    fn test_cursor_diagnostics_match_cursor_proto_shape() {
        let diagnostics = vec![zeta_prompt::ActiveBufferDiagnostic {
            severity: Some(1),
            message: "Cannot find name `nullthrows`.".to_string(),
            snippet: "const value = nullthrows(foo);\n".to_string(),
            snippet_buffer_row_range: 10..11,
            diagnostic_range_in_snippet: 14..24,
        }];

        assert_eq!(
            cursor_diagnostics(&diagnostics),
            json!([
                {
                    "message": "Cannot find name `nullthrows`.",
                    "range": {
                        "startLine": 10,
                        "startColumn": 14,
                        "endLine": 10,
                        "endColumn": 24,
                    },
                    "severity": 1,
                    "relatedInformation": [],
                }
            ])
        );
        assert_eq!(
            cursor_linter_errors(
                "src/file.ts",
                "const value = nullthrows(foo);\n",
                &diagnostics
            ),
            json!({
                "relativeWorkspacePath": "src/file.ts",
                "errors": [
                    {
                        "message": "Cannot find name `nullthrows`.",
                        "range": {
                            "startLine": 10,
                            "startColumn": 14,
                            "endLine": 10,
                            "endColumn": 24,
                        },
                        "relatedInformation": [],
                        "severity": 1,
                        "isStale": false,
                    }
                ],
                "fileContents": "const value = nullthrows(foo);\n",
            })
        );
    }

    #[test]
    fn test_cursor_lsp_suggested_items_match_cursor_proto_shape() {
        assert_eq!(
            cursor_lsp_suggested_items(&[
                "nullthrows".to_string(),
                " ".to_string(),
                "useMemo".to_string()
            ]),
            json!({
                "suggestions": [
                    { "label": "nullthrows" },
                    { "label": "useMemo" },
                ]
            })
        );
    }

    #[gpui::test]
    async fn test_external_jump_preserves_should_retrigger(cx: &mut TestAppContext) {
        init_test(cx);

        let buffer = cx.new(|cx| Buffer::local("one\ntwo\n", cx));
        let (snapshot, target) = buffer.read_with(cx, |buffer, _cx| {
            let snapshot = buffer.snapshot();
            let target = snapshot.anchor_before(language::Point::new(1, 0));
            (snapshot, target)
        });

        for should_retrigger in [false, true] {
            let prediction = CurrentExternalPrediction::Jump {
                id: Some(Arc::from("jump-id")),
                snapshot: snapshot.clone(),
                target,
                should_retrigger,
            };
            let edit_prediction =
                edit_prediction_from_current_prediction(&prediction, &snapshot).unwrap();

            match edit_prediction {
                EditPrediction::Jump {
                    should_retrigger: actual,
                    ..
                } => assert_eq!(actual, should_retrigger),
                EditPrediction::Local { .. } => panic!("expected jump prediction"),
            }
        }
    }

    #[test]
    fn test_import_code_action_selection() {
        assert!(is_import_code_action(
            Some(&lsp::CodeActionKind::QUICKFIX),
            "Add import from \"shared-utils\"",
            false,
        ));
        assert!(is_import_code_action(
            Some(&lsp::CodeActionKind::new("source.addMissingImports.ts")),
            "Add all missing imports",
            false,
        ));
        assert!(is_import_code_action(
            Some(&lsp::CodeActionKind::new("source.addMissingImports.ts")),
            "Apply source action",
            false,
        ));
        assert!(!is_import_code_action(
            Some(&lsp::CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
            "Organize Imports",
            false,
        ));
        assert!(!is_import_code_action(
            Some(&lsp::CodeActionKind::SOURCE_FIX_ALL),
            "Fix all auto-fixable problems",
            false,
        ));
        assert!(!is_import_code_action(
            Some(&lsp::CodeActionKind::QUICKFIX),
            "Remove unused import",
            false,
        ));
        assert!(!is_import_code_action(
            Some(&lsp::CodeActionKind::REFACTOR),
            "Add import",
            false,
        ));
        assert!(!is_import_code_action(
            Some(&lsp::CodeActionKind::QUICKFIX),
            "Add import",
            true,
        ));
    }

    fn test_code_action(title: &str, kind: lsp::CodeActionKind) -> CodeAction {
        let buffer_id = text::BufferId::new(1).unwrap();
        CodeAction {
            server_id: LanguageServerId(0),
            range: language::Anchor::min_for_buffer(buffer_id)
                ..language::Anchor::min_for_buffer(buffer_id),
            lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                title: title.into(),
                kind: Some(kind),
                ..Default::default()
            })),
            resolved: true,
        }
    }

    #[test]
    fn test_import_quick_fix_requests_quickfix_and_source_actions() {
        let kinds = import_quick_fix_code_action_kinds();
        assert_eq!(
            kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
            vec![
                lsp::CodeActionKind::QUICKFIX.as_str(),
                lsp::CodeActionKind::SOURCE.as_str()
            ]
        );
    }

    #[test]
    fn test_select_import_quick_fix_skips_source_actions_that_are_not_imports() {
        let selected = select_import_quick_fix(vec![
            test_code_action(
                "Organize Imports",
                lsp::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
            ),
            test_code_action(
                "Fix all auto-fixable problems",
                lsp::CodeActionKind::SOURCE_FIX_ALL,
            ),
            test_code_action(
                "Add import from \"shared-utils\"",
                lsp::CodeActionKind::QUICKFIX,
            ),
        ])
        .expect("should select the import-looking action");

        assert_eq!(
            selected.lsp_action.title(),
            "Add import from \"shared-utils\""
        );
    }

    #[test]
    fn test_import_quick_fix_retry_selects_first_import_after_lsp_settles() {
        let selected = select_import_quick_fix_from_attempts([
            vec![],
            vec![
                test_code_action(
                    "Organize Imports",
                    lsp::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                ),
                test_code_action(
                    "Fix all auto-fixable problems",
                    lsp::CodeActionKind::SOURCE_FIX_ALL,
                ),
            ],
            vec![test_code_action(
                "Add import from \"shared-utils\"",
                lsp::CodeActionKind::QUICKFIX,
            )],
        ])
        .expect("should select the import action from the first settled attempt");

        assert_eq!(selected.0, 2);
        assert_eq!(
            selected.1.lsp_action.title(),
            "Add import from \"shared-utils\""
        );
    }

    #[test]
    fn test_select_import_quick_fix_prefers_specific_imports() {
        let selected = select_import_quick_fix(vec![
            test_code_action(
                "Add all missing imports",
                lsp::CodeActionKind::new("source.addMissingImports.ts"),
            ),
            test_code_action(
                "Import 'nullthrows' from module \"shared-utils\"",
                lsp::CodeActionKind::QUICKFIX,
            ),
            test_code_action(
                "Add import from \"shared-utils\"",
                lsp::CodeActionKind::QUICKFIX,
            ),
        ])
        .expect("should select the most specific import quick fix");

        assert_eq!(
            selected.lsp_action.title(),
            "Import 'nullthrows' from module \"shared-utils\""
        );
    }

    #[gpui::test]
    async fn test_accepted_edit_search_range_tracks_inserted_prediction(cx: &mut TestAppContext) {
        init_test(cx);

        let buffer = cx.new(|cx| Buffer::local("const value = \n", cx));
        let prediction_edits = buffer.update(cx, |buffer, cx| {
            let snapshot = buffer.snapshot();
            let start = snapshot.anchor_before(language::Point::new(0, 14));
            let end = snapshot.anchor_after(language::Point::new(0, 14));
            let edits: Arc<[_]> = vec![(start..end, Arc::<str>::from("nullthrows(foo);\n"))].into();
            assert_eq!(
                accepted_edit_search_range(&snapshot, &edits),
                language::Point::new(0, 0)..language::Point::new(1, 0)
            );
            buffer.edit(edits.iter().cloned(), None, cx);
            edits
        });

        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            assert_eq!(
                accepted_edit_search_range(&snapshot, &prediction_edits),
                language::Point::new(0, 0)..language::Point::new(2, 0)
            );
        });
    }
}
