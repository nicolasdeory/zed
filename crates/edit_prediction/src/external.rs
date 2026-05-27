use crate::{EditPredictionStore, zeta::zeta2_prompt_input};
use anyhow::{Context as _, Result};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    interpolate_edits,
};
use futures::AsyncReadExt as _;
use gpui::{
    App, AsyncApp, Context, Entity, Task,
    http_client::{self, AsyncBody, HttpClient},
};
use icons::IconName;
use language::{Anchor, Buffer, BufferSnapshot, EditPreview, Point, ToOffset as _, ToPoint as _};
use project::Project;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use text::Bias;

#[derive(Clone)]
enum CurrentExternalPrediction {
    Local {
        id: Option<Arc<str>>,
        snapshot: BufferSnapshot,
        edits: Arc<[(Range<Anchor>, Arc<str>)]>,
        edit_preview: EditPreview,
    },
    Jump {
        id: Option<Arc<str>>,
        snapshot: BufferSnapshot,
        target: Anchor,
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

            let request = cx.update(|cx| {
                build_request(
                    &project,
                    &edit_prediction_store,
                    &snapshot,
                    cursor_position,
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

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.current_prediction = None;
        self.pending_request = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.current_prediction = None;
        self.pending_request = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        match self.current_prediction.as_ref()? {
            CurrentExternalPrediction::Local {
                id,
                snapshot,
                edits,
                edit_preview,
            } => {
                let buffer = buffer.read(cx);
                let edits = interpolate_edits(snapshot, &buffer.snapshot(), edits)?;
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
            } => Some(EditPrediction::Jump {
                id: id.as_ref().map(|id| id.to_string().into()),
                snapshot: snapshot.clone(),
                target: *target,
            }),
        }
    }
}

fn build_request(
    project: &Entity<Project>,
    edit_prediction_store: &Entity<EditPredictionStore>,
    snapshot: &BufferSnapshot,
    cursor_position: Anchor,
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
    let additional_files = cursor_additional_files(&related_files);
    let code_results = cursor_code_results(&related_files);
    let linter_errors = cursor_linter_errors(
        &relative_path,
        contents,
        &prompt_input.active_buffer_diagnostics,
    );

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
            "diagnostics": [],
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
        "lspSuggestedItems": { "suggestions": [] },
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
        let target = snapshot.anchor_before(point_for_position(&snapshot, jump.position));
        (snapshot, target)
    });

    Ok(Some(CurrentExternalPrediction::Jump {
        id: id.map(Into::into),
        snapshot,
        target,
    }))
}

fn point_for_position(snapshot: &BufferSnapshot, position: ExternalPosition) -> Point {
    snapshot.clip_point(Point::new(position.line, position.column), Bias::Left)
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
