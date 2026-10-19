use crate::{
    config::Settings,
    error::{Fault, Result},
};
use serde_json::{Value, json};
use std::collections::HashSet;

const GROUPS: [&str; 5] = [
    "web_search",
    "artifacts",
    "repl",
    "widgets",
    "project_knowledge",
];
const WIDGETS: [&str; 37] = [
    "weather_fetch",
    "display_stock_data",
    "recipe_display_v0",
    "places_search",
    "places_search_v1",
    "places_map_display_v0",
    "places_map_display_v1",
    "message_compose_v1",
    "writing_draft_v0",
    "ask_user_input_v0",
    "ask_user_free_form_input_v0",
    "fetch_sports_data",
    "recommend_claude_apps",
    "show_recommendation_cards",
    "quiz_display_v0",
    "storyboard_v0",
    "chart_display_v0",
    "options_card_display_v0",
    "step_card_display_v0",
    "itinerary_display_v0",
    "translation_display_v0",
    "comparison_card_display_v0",
    "featured_card_display_v0",
    "product_carousel_display_v0",
    "link_preview_display_v0",
    "places_list_display_v0",
    "starter_tasks_display_v0",
    "claude_code_install_display_v0",
    "role_select_display_v0",
    "approved_connectors_display_v0",
    "persona_cards_display_v0",
    "email_consent_prompt_v0",
    "insight_widget_v0",
    "prompts_card_display_v0",
    "source_card_display_v0",
    "offer_next_step_v0",
    "offer_schedule_v0",
];

fn enabled(group: &str, settings: &Settings) -> bool {
    match group {
        "web_search" => settings.native_web_search,
        "artifacts" => settings.native_artifacts,
        "repl" => settings.native_repl,
        "widgets" => settings.native_widgets,
        "project_knowledge" => settings.native_project_knowledge,
        _ => false,
    }
}

/// Parses an explicitly bound Claude project UUID in canonical form.
pub fn project(raw: &Value) -> Result<Option<String>> {
    let Some(value) = raw.get("claud2api_project_uuid") else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| Fault::invalid("claud2api_project_uuid must be a canonical UUID"))?;
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| Fault::invalid("claud2api_project_uuid must be a canonical UUID"))?;
    if parsed.to_string() != value {
        return Err(Fault::invalid(
            "claud2api_project_uuid must be a canonical UUID",
        ));
    }
    Ok(Some(value.to_owned()))
}

/// Resolves enabled groups from a Messages payload. Project knowledge is
/// included by default only when a project UUID is explicitly bound.
/// An explicit array selects only those groups and cannot override settings.
pub fn resolve(raw: &Value, settings: &Settings) -> Result<Vec<String>> {
    let project = project(raw)?;
    if project.is_some() && !settings.native_project_knowledge {
        return Err(Fault::invalid(
            "project binding is disabled by gateway settings",
        ));
    }
    let requested = match raw.get("claud2api_tools") {
        None => GROUPS
            .iter()
            .copied()
            .filter(|group| {
                enabled(group, settings) && (*group != "project_knowledge" || project.is_some())
            })
            .collect::<Vec<_>>(),
        Some(value) => value
            .as_array()
            .filter(|items| items.len() <= GROUPS.len())
            .ok_or_else(|| {
                Fault::invalid("claud2api_tools must be an array of native tool groups")
            })?
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|group| GROUPS.contains(group))
                    .ok_or_else(|| Fault::invalid("unsupported claud2api_tools group"))
            })
            .collect::<Result<Vec<_>>>()?,
    };

    if project.is_none() && requested.contains(&"project_knowledge") {
        return Err(Fault::invalid(
            "project_knowledge requires claud2api_project_uuid",
        ));
    }

    let mut selected = HashSet::new();
    for group in requested {
        if !selected.insert(group) {
            return Err(Fault::invalid("duplicate claud2api_tools group"));
        }
        if !enabled(group, settings) {
            return Err(Fault::invalid(
                "requested native tool group is disabled by gateway settings",
            ));
        }
    }
    Ok(GROUPS
        .iter()
        .filter(|group| selected.contains(**group))
        .map(|group| (*group).to_owned())
        .collect())
}

/// Builds the first-party tool descriptors in the gateway's stable order.
pub fn descriptors(groups: &[String]) -> Value {
    let mut out = Vec::new();
    for group in groups {
        match group.as_str() {
            "web_search" => out.push(json!({"type":"web_search_v0","name":"web_search"})),
            "artifacts" => out.push(json!({"type":"artifacts_v0","name":"artifacts"})),
            "repl" => {
                out.push(json!({"type":"repl_v0","name":"repl"}));
                out.push(json!({"type":"send_to_user_v0","name":"send_to_user"}));
            }
            "widgets" => out.extend(
                WIDGETS
                    .iter()
                    .map(|name| json!({"type":"widget","name":name})),
            ),
            "project_knowledge" => out
                .push(json!({"type":"project_knowledge_search","name":"project_knowledge_search"})),
            _ => {}
        }
    }
    Value::Array(out)
}

/// Lists native names that the upstream may emit for the resolved groups.
/// Search may call its own fetch and fast-search subtools without separate
/// top-level descriptors.
pub fn allowed_names(groups: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    for group in groups {
        match group.as_str() {
            "web_search" => {
                names.extend(["web_search", "web_search_fast", "web_fetch", "image_search"])
            }
            "artifacts" => names.extend([
                "Artifact",
                "artifacts",
                "create_artifact",
                "create_file",
                "str_replace",
                "present_files",
            ]),
            "repl" => names.extend([
                "repl",
                "bash_tool",
                "create_file",
                "str_replace",
                "str_replace_editor",
                "read",
                "write",
                "edit",
                "open_file",
                "update_file",
                "view",
                "present_files",
                "file_search",
                "delete_file",
                "kill_bash",
                "send_to_user",
            ]),
            "widgets" => names.extend(WIDGETS.iter().copied()),
            "project_knowledge" => names.push("project_knowledge_search"),
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(*name))
        .map(str::to_owned)
        .collect()
}

const CONTROL_FIELDS: &[&str] = &[
    "approval_key",
    "approval_key_legacy",
    "approval_options",
    "mcp_server_url",
];

fn strip_metadata_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in CONTROL_FIELDS {
                object.remove(*key);
            }
            for value in object.values_mut() {
                strip_metadata_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_metadata_fields(value);
            }
        }
        _ => {}
    }
}

/// Removes upstream control credentials from native block metadata while
/// leaving user supplied tool input and result content untouched.
pub fn sanitize(mut block: Value) -> Value {
    if let Some(object) = block.as_object_mut() {
        for key in CONTROL_FIELDS {
            object.remove(*key);
        }
        // These fields carry UI metadata. `input`, `content`, and
        // `structured_content` are user/model data and are intentionally not
        // traversed, even if their payload happens to use a similarly named key.
        for key in ["meta", "flags"] {
            if let Some(value) = object.get_mut(key) {
                strip_metadata_fields(value);
            }
        }
    }
    block
}
