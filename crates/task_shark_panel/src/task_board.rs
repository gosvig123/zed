use std::path::PathBuf;

use agent_ui::thread_metadata_store::ThreadId;
use anyhow::{Context as _, Result, bail};
use db::kvp::KeyValueStore;
use serde::{Deserialize, Serialize};

use crate::run_command;

pub(crate) const BOARD_SOURCE: &str = "zed";
pub(crate) const BOARD_FILE_NAME: &str = "board.jsonl";
const BOARD_PAGE_LIMIT: &str = "100";
const RESOURCE_DIRECTORY_VARIABLE: &str = "TASKSHARK_MCP_RESOURCE_DIR";
const BOARD_ROOT_VARIABLE: &str = "TASKSHARK_BOARD_ROOT";
const TASK_ID_VARIABLE: &str = "TASKSHARK_TASK_ID";
const ACTOR_KIND_VARIABLE: &str = "TASKSHARK_ACTOR_KIND";
const HUMAN_ACTOR: &str = "human";
const NOTE_KIND: &str = "note";
pub(crate) const KIND_PROGRESS: &str = "progress";
pub(crate) const KIND_DECISION: &str = "decision";
pub(crate) const KIND_BLOCKER: &str = "blocker";
pub(crate) const KIND_HANDOFF: &str = "handoff";

// Mirrors the version 1 Board Update entries written by TaskBoardMCP (`store.mjs`).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BoardEntry {
    pub sequence: u64,
    pub created_at: String,
    pub kind: String,
    pub body: String,
    pub actor_kind: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(rename = "threadID", default)]
    pub thread_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Board {
    pub entries: Vec<BoardEntry>,
    pub has_more: bool,
}

/// Connects a Zed agent thread to the conversation ID that thread uses on the task board.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ThreadLink {
    pub conversation_id: String,
    pub thread_id: ThreadId,
}

pub(crate) fn board_root() -> PathBuf {
    std::env::var_os(BOARD_ROOT_VARIABLE)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            util::paths::home_dir().join("Library/Application Support/TaskShark/SharedTasks/v1")
        })
}

pub(crate) async fn read_board(task_id: &str) -> Result<Board> {
    let output = run_board_cli(
        &["board-read", "--limit", BOARD_PAGE_LIMIT],
        &[(TASK_ID_VARIABLE, task_id)],
        None,
    )
    .await?;
    serde_json::from_slice(&output).context("parsing `cli.mjs board-read` output")
}

pub(crate) async fn post_note(task_id: &str, body: String) -> Result<()> {
    run_board_cli(
        &["board-post", "--kind", NOTE_KIND, "--source", BOARD_SOURCE],
        &[
            (TASK_ID_VARIABLE, task_id),
            (ACTOR_KIND_VARIABLE, HUMAN_ACTOR),
        ],
        Some(body.into_bytes()),
    )
    .await?;
    Ok(())
}

pub(crate) fn read_links(store: &KeyValueStore, task_id: &str) -> Result<Vec<ThreadLink>> {
    let Some(value) = store.read_kvp(&links_key(task_id))? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&value).context("parsing saved Task Shark thread links")
}

pub(crate) async fn add_link(
    store: KeyValueStore,
    task_id: String,
    link: ThreadLink,
) -> Result<()> {
    let mut links = read_links(&store, &task_id)?;
    links.push(link);
    store
        .write_kvp(links_key(&task_id), serde_json::to_string(&links)?)
        .await
}

fn links_key(task_id: &str) -> String {
    format!("task_shark_panel_thread_links:{task_id}")
}

async fn run_board_cli(
    arguments: &[&str],
    environment: &[(&str, &str)],
    input: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let node = find_node()?;
    let resource_directory = std::env::var_os(RESOURCE_DIRECTORY_VARIABLE)
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join("Developer/tasks-widget/TaskBoardMCP"));
    let cli = resource_directory.join("cli.mjs");
    if !cli.is_file() {
        bail!(
            "Task Shark board CLI not found at {}. Set {RESOURCE_DIRECTORY_VARIABLE} to the TaskBoardMCP folder",
            cli.display()
        );
    }
    let cli = cli.to_string_lossy().into_owned();
    let mut command_arguments = vec![cli.as_str()];
    command_arguments.extend_from_slice(arguments);
    run_command(&node, &command_arguments, environment, input).await
}

pub(crate) fn find_node() -> Result<PathBuf> {
    // GUI launches do not inherit the login shell PATH, so probe common install locations first.
    let home = util::paths::home_dir();
    [
        PathBuf::from("/opt/homebrew/bin/node"),
        PathBuf::from("/usr/local/bin/node"),
        home.join(".local/share/mise/shims/node"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .or_else(|| which::which("node").ok())
    .context("`node` not found in /opt/homebrew/bin, /usr/local/bin, mise shims, or PATH")
}
