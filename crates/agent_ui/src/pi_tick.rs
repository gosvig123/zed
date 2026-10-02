use std::{ffi::OsStr, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use util::{
    command::{Stdio, new_command},
    shell::ShellKind,
};

const DATA_DIRECTORY_VARIABLE: &str = "PI_TICK_DATA_DIR";
pub(crate) const JOBS_FILE_NAME: &str = "jobs.json";
pub(crate) const RUNS_FILE_NAME: &str = "runs.jsonl";
pub(crate) const ACTIVE_DIRECTORY_NAME: &str = "active";
/// A JSON array of SSH host aliases whose ticks are shown next to this machine's.
pub(crate) const HOSTS_FILE_NAME: &str = "hosts.json";
const RECENT_RUN_LIMIT: usize = 20;

// A Zed-owned control socket keeps one connection per host open between polls, and
// disabling LocalCommand keeps per-host connection hooks in ~/.ssh/config from running.
const SSH_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=8",
    "-o",
    "ControlMaster=auto",
    "-o",
    "ControlPath=~/.ssh/zed-tick-%C",
    "-o",
    "ControlPersist=10m",
    "-o",
    "PermitLocalCommand=no",
];

// Non-interactive SSH sessions do not load the login PATH, so probe the same places as
// `find_node`. Sets `$N` (node) and `$D` (pi-tick data directory) for the script that follows.
const REMOTE_PRELUDE: &str = r#"D="${PI_TICK_DATA_DIR:-$HOME/.pi/agent/tick}"
N=""
for candidate in "$(command -v node 2>/dev/null)" /opt/homebrew/bin/node /usr/local/bin/node "$HOME/.local/share/mise/shims/node"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then N="$candidate"; break; fi
done
if [ -z "$N" ]; then echo "node not found on $(hostname)" >&2; exit 127; fi
if [ ! -f "$D/pi-tick.mjs" ]; then echo "pi-tick CLI not found at $D/pi-tick.mjs on $(hostname)" >&2; exit 127; fi"#;

// Prints the job ids of live runs as one JSON line, like `running_job_ids` does locally.
const REMOTE_RUNNING_SCRIPT: &str = r#"const fs = require("fs"), path = require("path");
const directory = process.argv[1];
const ids = [];
let names = [];
try { names = fs.readdirSync(directory); } catch {}
for (const name of names) {
  if (!name.endsWith(".json")) continue;
  let record;
  try { record = JSON.parse(fs.readFileSync(path.join(directory, name), "utf8")); } catch { continue; }
  try { process.kill(record.pid, 0); ids.push(record.jobId); }
  catch (error) { if (error.code === "EPERM") ids.push(record.jobId); }
}
console.log(JSON.stringify(ids));"#;

// pi-tick has no edit command, so creating and editing run this script on the tick's host. It
// uses pi-tick's own modules: `createTick` for new ticks, and for edits the same validation,
// catalog lock, and backend unregister/enable that `disable` and `enable` use. Editing keeps
// the run history and transcripts, which delete-and-add would remove.
const SAVE_SCRIPT: &str = r#"import { pathToFileURL } from "node:url";
const [directory, rawDraft] = process.argv.slice(1);
const base = pathToFileURL(directory + "/");
const load = (file) => import(new URL(file, base).href);
try {
  const draft = JSON.parse(rawDraft);
  const scheduleOpts = { time: draft.time, days: draft.days, minutes: draft.minutes };
  if (draft.create) {
    const { createTick } = await load("commands/add.mjs");
    await createTick({
      jobId: draft.id,
      prompt: draft.prompt,
      cwd: draft.cwd,
      scheduleKind: draft.scheduleKind,
      scheduleOpts,
      enabled: draft.enabled,
      model: draft.model,
    });
  } else {
    const { ensureDataDirs, loadCatalog, saveCatalog, findJob, withCatalogLock, PiTickError } = await load("catalog.mjs");
    const { validatePrompt, validateCwd } = await load("validate.mjs");
    const { buildSchedule } = await load("schedule.mjs");
    const { activeBackend } = await load("backend-info.mjs");
    const { cmdEnableInternal } = await load("commands/enable.mjs");
    ensureDataDirs();
    const before = findJob(loadCatalog(), draft.id);
    if (!before) throw new PiTickError(`no such job: ${draft.id}`, 4);
    validatePrompt(draft.prompt);
    validateCwd(draft.cwd);
    const schedule = buildSchedule(draft.scheduleKind, scheduleOpts);
    if (before.enabled) {
      const result = await activeBackend().unregister(draft.id);
      if (!result.ok) throw new PiTickError(result.error, 1);
    }
    await withCatalogLock(async () => {
      const catalog = loadCatalog();
      const job = findJob(catalog, draft.id);
      if (!job) throw new PiTickError(`no such job: ${draft.id}`, 4);
      Object.assign(job, {
        prompt: draft.prompt,
        cwd: draft.cwd,
        schedule,
        model: draft.model,
        enabled: false,
        updatedAt: new Date().toISOString(),
      });
      saveCatalog(catalog);
    });
    if (before.enabled) {
      try {
        await cmdEnableInternal(draft.id);
      } catch (error) {
        error.message = `saved, but the tick is now disabled because enabling failed: ${error.message}`;
        throw error;
      }
    }
  }
} catch (error) {
  process.stderr.write(`pi-tick: ${error.message}\n`);
  process.exit(Number.isInteger(error.code) ? error.code : 1);
}"#;

/// The fields a person can set when creating or editing a tick. Schedule fields are kept as
/// typed text so pi-tick validates them exactly as it validates its own CLI flags.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TickDraft {
    pub id: String,
    pub prompt: String,
    pub cwd: String,
    pub schedule_kind: String,
    pub time: String,
    pub days: String,
    pub minutes: String,
    pub model: Option<String>,
    /// Only used when creating; editing keeps the current enabled state.
    pub enabled: bool,
    pub create: bool,
}

/// The machine that owns a tick's schedule, runs, and transcripts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum TickHost {
    #[default]
    Local,
    Ssh(String),
}

impl TickHost {
    pub fn label(&self) -> &str {
        match self {
            TickHost::Local => "This machine",
            TickHost::Ssh(alias) => alias,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, TickHost::Ssh(_))
    }
}

/// Job ids are only unique per host, because migrated jobs keep their ids.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TickKey {
    pub host: TickHost,
    pub job_id: String,
}

pub(crate) struct HostFailure {
    pub host: TickHost,
    pub error: anyhow::Error,
}

pub(crate) struct TickOverview {
    pub hosts: Vec<TickHost>,
    pub jobs: Vec<TickJob>,
    pub failures: Vec<HostFailure>,
}

// Mirrors the version 1 catalog written by pi-tick (`catalog.mjs`).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TickJob {
    pub id: String,
    pub prompt: String,
    pub cwd: String,
    pub schedule: Schedule,
    pub enabled: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub last_run: Option<LastRun>,
    #[serde(skip)]
    pub running: bool,
    #[serde(skip)]
    pub host: TickHost,
}

#[derive(Clone, Deserialize)]
pub(crate) struct Schedule {
    pub kind: String,
    #[serde(default)]
    pub value: ScheduleValue,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduleValue {
    #[serde(default)]
    pub minutes: Option<u64>,
    #[serde(default)]
    pub seconds: Option<u64>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub days: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastRun {
    pub finished_at: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub final_text_preview: Option<String>,
}

// Mirrors one line of pi-tick's run history (`runs.jsonl`).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunRecord {
    pub run_id: String,
    pub job_id: String,
    pub trigger_kind: String,
    pub started_at: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub final_text_preview: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveRun {
    job_id: String,
    pid: i64,
}

impl TickJob {
    pub fn key(&self) -> TickKey {
        TickKey {
            host: self.host.clone(),
            job_id: self.id.clone(),
        }
    }

    pub fn status(&self) -> &'static str {
        if self.running {
            "Running"
        } else if self.enabled {
            "Active"
        } else {
            "Disabled"
        }
    }

    fn rank(&self) -> u8 {
        match (self.running, self.enabled) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => 2,
        }
    }
}

impl Schedule {
    pub fn describe(&self) -> String {
        let time = self.value.time.as_deref().unwrap_or("?");
        match self.kind.as_str() {
            "daily" => format!("Daily at {time}"),
            "weekly" => format!("Weekly on {} at {time}", self.value.days.join(", ")),
            "interval" => {
                let seconds =
                    self.value.minutes.unwrap_or(0) * 60 + self.value.seconds.unwrap_or(0);
                if seconds.is_multiple_of(60) {
                    format!("Every {} min", seconds / 60)
                } else {
                    format!("Every {seconds} s")
                }
            }
            kind => kind.to_string(),
        }
    }
}

impl RunRecord {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && self.error.is_none()
    }
}

pub(crate) fn data_directory() -> PathBuf {
    std::env::var_os(DATA_DIRECTORY_VARIABLE)
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".pi/agent/tick"))
}

pub(crate) fn configured_hosts() -> Result<Vec<TickHost>> {
    let path = data_directory().join(HOSTS_FILE_NAME);
    let content = match std::fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![TickHost::Local]);
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let aliases: Vec<String> = serde_json::from_slice(&content).with_context(|| {
        format!(
            "parsing {} (expected a JSON array of SSH host aliases, such as [\"devbox\"])",
            path.display()
        )
    })?;
    let mut hosts = vec![TickHost::Local];
    for alias in aliases {
        // A leading dash would be read by ssh as an option.
        if alias.is_empty() || alias.starts_with('-') || alias.contains(char::is_whitespace) {
            bail!("invalid SSH host alias {alias:?} in {}", path.display());
        }
        hosts.push(TickHost::Ssh(alias));
    }
    Ok(hosts)
}

/// Loads every configured host; an unreachable host is reported without hiding the others.
pub(crate) async fn load_jobs() -> Result<TickOverview> {
    let hosts = configured_hosts()?;
    let results = futures::future::join_all(hosts.iter().map(load_host_jobs)).await;
    let mut jobs = Vec::new();
    let mut failures = Vec::new();
    for (host, result) in hosts.iter().zip(results) {
        match result {
            Ok(host_jobs) => jobs.extend(host_jobs),
            Err(error) => failures.push(HostFailure {
                host: host.clone(),
                error,
            }),
        }
    }
    Ok(TickOverview {
        hosts,
        jobs,
        failures,
    })
}

async fn load_host_jobs(host: &TickHost) -> Result<Vec<TickJob>> {
    let (mut jobs, running) = match host {
        TickHost::Local => (
            parse_jobs(&run_local_cli(&["list", "--json"]).await?)?,
            running_job_ids(),
        ),
        TickHost::Ssh(alias) => {
            let script = format!(
                "{} || exit $?\nprintf '\\n'\n\"$N\" -e {} \"$D/{ACTIVE_DIRECTORY_NAME}\"",
                cli_invocation(&["list", "--json"])?,
                quote(REMOTE_RUNNING_SCRIPT)?,
            );
            let output = String::from_utf8(run_ssh(alias, &script).await?)
                .context("reading pi-tick output")?;
            let (list, running) = output
                .trim_end()
                .rsplit_once('\n')
                .context("pi-tick output is missing the running-jobs line")?;
            let running: Vec<String> =
                serde_json::from_str(running).context("parsing running jobs")?;
            (parse_jobs(list.as_bytes())?, running)
        }
    };
    for job in &mut jobs {
        job.host = host.clone();
        job.running = running.contains(&job.id);
    }
    jobs.sort_by(|left, right| left.rank().cmp(&right.rank()).then(left.id.cmp(&right.id)));
    Ok(jobs)
}

fn parse_jobs(output: &[u8]) -> Result<Vec<TickJob>> {
    serde_json::from_slice(output).context("parsing `pi-tick list --json` output")
}

pub(crate) async fn recent_runs(key: &TickKey) -> Result<Vec<RunRecord>> {
    let content = match &key.host {
        TickHost::Local => {
            let path = data_directory().join(RUNS_FILE_NAME);
            match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("reading {}", path.display()));
                }
            }
        }
        TickHost::Ssh(alias) => {
            // Filtering on the host keeps the transfer small; pi-tick writes compact JSON.
            let pattern = format!("\"jobId\":\"{}\"", key.job_id);
            let script = format!(
                "grep -F -- {} \"$D/{RUNS_FILE_NAME}\" 2>/dev/null | tail -n {RECENT_RUN_LIMIT}",
                quote(&pattern)?
            );
            String::from_utf8_lossy(&run_ssh(alias, &script).await?).into_owned()
        }
    };
    // pi-tick appends one record per line; a torn final line during a write is skipped.
    Ok(content
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<RunRecord>(line).ok())
        .filter(|run| run.job_id == key.job_id)
        .take(RECENT_RUN_LIMIT)
        .collect())
}

pub(crate) async fn set_enabled(key: &TickKey, enabled: bool) -> Result<()> {
    let verb = if enabled { "enable" } else { "disable" };
    run_cli(&key.host, &[verb, &key.job_id]).await?;
    Ok(())
}

/// Locally this lasts as long as the agent run. On a remote host the run is detached, so a
/// dropped connection or a sleeping Mac does not stop it; only start-up failures are returned.
pub(crate) async fn run_now(key: &TickKey) -> Result<()> {
    let job_id = key.job_id.as_str();
    if let TickHost::Ssh(alias) = &key.host {
        let script = format!(
            r#"log=$(mktemp "${{TMPDIR:-/tmp}}/pi-tick-run.XXXXXX") || exit 1
nohup {} </dev/null >/dev/null 2>"$log" &
pid=$!
sleep 3
if kill -0 "$pid" 2>/dev/null; then rm -f "$log"; exit 0; fi
wait "$pid"; exit_code=$?
cat "$log" >&2; rm -f "$log"
exit "$exit_code""#,
            cli_invocation(&["run", job_id, "--manual"])?
        );
        run_ssh(alias, &script).await?;
        return Ok(());
    }
    let output = new_command(find_node()?)
        .arg(cli_path()?)
        .args(["run", job_id, "--manual"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("starting `pi-tick run`")?;
    // The runner exits with the agent's exit code and records that outcome in the run
    // history, so only errors that pi-tick reports before or around the run are raised.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && !stderr.trim().is_empty() {
        bail!("{}", stderr.trim());
    }
    Ok(())
}

/// Creates a tick, or replaces an existing tick's prompt, directory, schedule, and model.
pub(crate) async fn save(host: &TickHost, draft: &TickDraft) -> Result<()> {
    let draft = serde_json::to_string(draft).context("encoding the tick")?;
    match host {
        TickHost::Local => {
            let directory = data_directory();
            cli_path()?;
            run_local_node([
                OsStr::new("--input-type=module"),
                OsStr::new("-e"),
                OsStr::new(SAVE_SCRIPT),
                directory.as_os_str(),
                OsStr::new(&draft),
            ])
            .await?;
        }
        TickHost::Ssh(alias) => {
            let script = format!(
                "\"$N\" --input-type=module -e {} \"$D\" {}",
                quote(SAVE_SCRIPT)?,
                quote(&draft)?
            );
            run_ssh(alias, &script).await?;
        }
    }
    Ok(())
}

pub(crate) async fn kill(key: &TickKey) -> Result<()> {
    run_cli(&key.host, &["kill", &key.job_id]).await?;
    Ok(())
}

pub(crate) async fn delete(key: &TickKey) -> Result<()> {
    run_cli(&key.host, &["delete", &key.job_id]).await?;
    Ok(())
}

pub(crate) async fn transcript(key: &TickKey, run_id: Option<&str>) -> Result<String> {
    let mut arguments = vec!["show", key.job_id.as_str()];
    if let Some(run_id) = run_id {
        arguments.extend(["--run-id", run_id]);
    }
    let output = run_cli(&key.host, &arguments).await?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn running_job_ids() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_directory().join(ACTIVE_DIRECTORY_NAME)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| std::fs::read(entry.ok()?.path()).ok())
        .filter_map(|content| serde_json::from_slice::<ActiveRun>(&content).ok())
        // A run that crashed leaves its record behind, so only live runner processes count.
        .filter(|run| process_is_alive(run.pid))
        .map(|run| run.job_id)
        .collect()
}

#[cfg(unix)]
fn process_is_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // Signal 0 only checks the process; EPERM still proves it exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: i64) -> bool {
    false
}

fn cli_path() -> Result<PathBuf> {
    let cli = data_directory().join("pi-tick.mjs");
    if !cli.is_file() {
        bail!(
            "pi-tick CLI not found at {}. Open a Pi session once to install it, or set {DATA_DIRECTORY_VARIABLE}",
            cli.display()
        );
    }
    Ok(cli)
}

async fn run_cli(host: &TickHost, arguments: &[&str]) -> Result<Vec<u8>> {
    match host {
        TickHost::Local => run_local_cli(arguments).await,
        TickHost::Ssh(alias) => run_ssh(alias, &cli_invocation(arguments)?).await,
    }
}

fn cli_invocation(arguments: &[&str]) -> Result<String> {
    let mut invocation = String::from("\"$N\" \"$D/pi-tick.mjs\"");
    for argument in arguments {
        invocation.push(' ');
        invocation.push_str(&quote(argument)?);
    }
    Ok(invocation)
}

fn quote(argument: &str) -> Result<String> {
    ShellKind::Posix
        .try_quote(argument)
        .map(|quoted| quoted.into_owned())
        .with_context(|| format!("cannot quote {argument:?} for a remote shell"))
}

async fn run_ssh(alias: &str, script: &str) -> Result<Vec<u8>> {
    let output = new_command("ssh")
        .args(SSH_OPTIONS)
        .arg(alias)
        .arg(format!("{REMOTE_PRELUDE}\n{script}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("starting ssh to {alias}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("ssh {alias} exited with {}", output.status);
        }
        bail!("{stderr}");
    }
    Ok(output.stdout)
}

async fn run_local_cli(arguments: &[&str]) -> Result<Vec<u8>> {
    let cli = cli_path()?;
    run_local_node(
        std::iter::once(cli.as_os_str())
            .chain(arguments.iter().map(|argument| OsStr::new(argument))),
    )
    .await
}

async fn run_local_node(arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Vec<u8>> {
    let output = new_command(find_node()?)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("starting pi-tick")?;
    if !output.status.success() {
        // pi-tick already prefixes its errors with "pi-tick:".
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("pi-tick exited with {}", output.status);
        }
        bail!("{stderr}");
    }
    Ok(output.stdout)
}

fn find_node() -> Result<PathBuf> {
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
